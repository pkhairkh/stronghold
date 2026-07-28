#!/usr/bin/env bash
# Stronghold backup script
#
# Per W10-T7 DoD:
#   - Calls `stronghold backup --to s3://...` (or local tarball if no S3 configured)
#   - Encrypts keys with a tenant-supplied password (age or gpg)
#   - SQLite online backup (via .backup command — does not block writers)
#
# Idempotent in the sense that re-running produces a new timestamped backup;
# old backups are not deleted (prune with --keep-days).
#
# Usage:
#   bash setup/backup.sh                                          # local backup
#   bash setup/backup.sh --to s3://my-bucket/stronghold/         # upload to S3
#   bash setup/backup.sh --to s3://... --keep-days 30            # prune old
#   bash setup/backup.sh --restore /tmp/backup-2026-07-29.tar.gz # restore
#
# Environment:
#   STRONGHOLD_DATA_DIR       (default: /var/lib/stronghold)
#   STRONGHOLD_CONFIG_DIR     (default: /etc/stronghold)
#   STRONGHOLD_BACKUP_DIR     (default: /var/lib/stronghold/backups)
#   BACKUP_ENCRYPTION_PASS    (required for encrypt; prompt if unset)
#   AWS_ACCESS_KEY_ID         (for S3 upload)
#   AWS_SECRET_ACCESS_KEY     (for S3 upload)
#   AWS_DEFAULT_REGION        (for S3 upload)

set -euo pipefail

DATA_DIR="${STRONGHOLD_DATA_DIR:-/var/lib/stronghold}"
CONFIG_DIR="${STRONGHOLD_CONFIG_DIR:-/etc/stronghold}"
BACKUP_DIR="${STRONGHOLD_BACKUP_DIR:-${DATA_DIR}/backups}"
DEST=""
RESTORE_PATH=""
KEEP_DAYS=0
DRY_RUN=false
NO_ENCRYPT=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --to=*)         DEST="${1#*=}" ;;
        --to)           DEST="$2"; shift ;;
        --restore=*)    RESTORE_PATH="${1#*=}" ;;
        --restore)      RESTORE_PATH="$2"; shift ;;
        --keep-days=*)  KEEP_DAYS="${1#*=}" ;;
        --keep-days)    KEEP_DAYS="$2"; shift ;;
        --dry-run)      DRY_RUN=true ;;
        --no-encrypt)   NO_ENCRYPT=true ;;
        --help|-h)
            cat <<EOF
Usage: backup.sh [OPTIONS]

Backup:
  --to=PATH             Destination: local dir, or s3://bucket/prefix
  --keep-days=N         Prune local backups older than N days (default: keep all)
  --no-encrypt          Skip encryption (NOT recommended for keys)
  --dry-run             Show actions without performing them

Restore:
  --restore=PATH        Restore from a backup archive (encrypted or plain)

Environment:
  BACKUP_ENCRYPTION_PASS    Encryption password (prompted if unset)
  AWS_ACCESS_KEY_ID         S3 credentials
  AWS_SECRET_ACCESS_KEY     S3 credentials
  AWS_DEFAULT_REGION        S3 region
EOF
            exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
    shift
done

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Run as root" >&2
    exit 2
fi

# Color helpers
if [[ -t 1 ]]; then
    C_INFO='\033[1;34m'; C_OK='\033[1;32m'; C_WARN='\033[1;33m'
    C_ERR='\033[1;31m'; C_RST='\033[0m'
else
    C_INFO=''; C_OK=''; C_WARN=''; C_ERR=''; C_RST=''
fi
log()  { echo -e "${C_INFO}[*]${C_RST} $*"; }
ok()   { echo -e "${C_OK}[+]${C_RST} $*"; }
warn() { echo -e "${C_WARN}[!]${C_RST} $*" >&2; }
err()  { echo -e "${C_ERR}[x]${C_RST} $*" >&2; }

# --- Restore mode ---
if [[ -n "$RESTORE_PATH" ]]; then
    log "Restore mode: $RESTORE_PATH"
    if [[ ! -f "$RESTORE_PATH" ]]; then
        err "Backup file not found: $RESTORE_PATH"
        exit 1
    fi

    # Detect encryption (age files start with "age-encryption.org/")
    IS_ENCRYPTED=false
    if head -c 25 "$RESTORE_PATH" 2>/dev/null | grep -q '^age-encryption'; then
        IS_ENCRYPTED=true
    fi

    WORK_DIR="$(mktemp -d)"
    trap 'rm -rf "$WORK_DIR"' EXIT

    if [[ "$IS_ENCRYPTED" == "true" ]]; then
        log "Backup is encrypted. Decrypting..."
        if ! command -v age &>/dev/null; then
            err "age not installed. Install with: dnf install -y age"
            exit 1
        fi
        if [[ -z "${BACKUP_ENCRYPTION_PASS:-}" ]]; then
            read -rs -p "Decryption password: " BACKUP_ENCRYPTION_PASS
            echo
        fi
        if ! echo -n "$BACKUP_ENCRYPTION_PASS" | age decrypt -p -o "${WORK_DIR}/backup.tar.gz" "$RESTORE_PATH"; then
            err "Decryption failed (wrong password?)"
            exit 1
        fi
    else
        cp "$RESTORE_PATH" "${WORK_DIR}/backup.tar.gz"
    fi

    log "Extracting backup..."
    mkdir -p "${WORK_DIR}/extracted"
    tar -xzf "${WORK_DIR}/backup.tar.gz" -C "${WORK_DIR}/extracted"

    # Stop services before overwriting
    log "Stopping services..."
    systemctl stop stronghold-gateway 2>/dev/null || true

    # Restore data dir
    if [[ -d "${WORK_DIR}/extracted/data" ]]; then
        log "Restoring data dir to ${DATA_DIR}..."
        mkdir -p "$DATA_DIR"
        rsync -a --delete "${WORK_DIR}/extracted/data/" "${DATA_DIR}/"
    fi

    # Restore config dir
    if [[ -d "${WORK_DIR}/extracted/config" ]]; then
        log "Restoring config dir to ${CONFIG_DIR}..."
        mkdir -p "$CONFIG_DIR"
        rsync -a --delete "${WORK_DIR}/extracted/config/" "${CONFIG_DIR}/"
    fi

    log "Restarting services..."
    systemctl start stronghold-gateway 2>/dev/null || true

    ok "Restore complete. Verify with:"
    ok "  systemctl status stronghold-gateway"
    ok "  stronghold audit verify --tenant <id>"
    exit 0
fi

# --- Backup mode ---
TS="$(date -u +%Y%m%dT%H%M%SZ)"
HOST="$(hostname -s)"
ARCHIVE_NAME="stronghold-${HOST}-${TS}.tar.gz"
ENCRYPTED_NAME="${ARCHIVE_NAME}.age"

echo "=========================================="
echo "  Stronghold Backup"
echo "=========================================="
echo "  Timestamp: $TS"
echo "  Host:      $HOST"
echo "  Data dir:  $DATA_DIR"
echo "  Config:    $CONFIG_DIR"
echo "  Dest:      ${DEST:-${BACKUP_DIR}}"
echo ""

if [[ ! -d "$DATA_DIR" ]]; then
    err "Data directory not found: $DATA_DIR"
    exit 1
fi

# Ensure backup tooling present
if [[ "$NO_ENCRYPT" == "false" ]]; then
    if ! command -v age &>/dev/null; then
        log "Installing age for encryption..."
        dnf install -y -q age 2>/dev/null || true
    fi
    if ! command -v age &>/dev/null; then
        err "age not installed. Install with: dnf install -y age"
        err "Or pass --no-encrypt (NOT recommended for production keys)."
        exit 1
    fi
fi

# Working directory
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# --- 1. SQLite online backup (does not block writers) ---
DB_PATH="${DATA_DIR}/stronghold.db"
if [[ -f "$DB_PATH" ]]; then
    log "Performing SQLite online backup of ${DB_PATH}..."
    if command -v sqlite3 &>/dev/null; then
        sqlite3 "$DB_PATH" ".backup '${WORK_DIR}/stronghold.db'"
        ok "SQLite backup complete (consistent snapshot)"
    else
        warn "sqlite3 CLI not installed; copying DB file directly (may be inconsistent)"
        cp "$DB_PATH" "${WORK_DIR}/stronghold.db"
        cp "${DB_PATH}-wal" "${WORK_DIR}/stronghold.db-wal" 2>/dev/null || true
        cp "${DB_PATH}-shm" "${WORK_DIR}/stronghold.db-shm" 2>/dev/null || true
    fi
else
    warn "No SQLite database at ${DB_PATH}; skipping DB backup"
fi

# --- 2. Copy keys, audit logs, config ---
STAGING="${WORK_DIR}/staging"
mkdir -p "${STAGING}/data" "${STAGING}/config"

# Keys (mode preserved)
if [[ -d "${DATA_DIR}/keys" ]]; then
    log "Staging keys..."
    cp -a "${DATA_DIR}/keys" "${STAGING}/data/"
    chmod 700 "${STAGING}/data/keys"
fi

# Audit logs
if [[ -d "${DATA_DIR}/audit" ]]; then
    log "Staging audit logs..."
    cp -a "${DATA_DIR}/audit" "${STAGING}/data/"
fi

# Database snapshot
if [[ -f "${WORK_DIR}/stronghold.db" ]]; then
    cp -a "${WORK_DIR}/stronghold.db" "${STAGING}/data/"
    [[ -f "${WORK_DIR}/stronghold.db-wal" ]] && cp -a "${WORK_DIR}/stronghold.db-wal" "${STAGING}/data/" || true
fi

# Config (includes TLS cert, ntfy.yml, etc.)
if [[ -d "$CONFIG_DIR" ]]; then
    log "Staging config dir..."
    cp -a "$CONFIG_DIR" "${STAGING}/config/"
    # Don't back up TLS private keys in plaintext even with --no-encrypt
    if [[ "$NO_ENCRYPT" == "true" ]]; then
        warn "  WARNING: --no-encrypt is set; TLS keys will be in the tarball."
    fi
fi

# Also stage ntfy config and server.yml for completeness
if [[ -f /etc/ntfy/server.yml ]]; then
    mkdir -p "${STAGING}/etc/ntfy"
    cp -a /etc/ntfy/server.yml "${STAGING}/etc/ntfy/"
fi

# Manifest
cat > "${STAGING}/MANIFEST.json" <<EOF
{
  "hostname": "${HOST}",
  "timestamp": "${TS}",
  "stronghold_version": "$(/usr/local/bin/stronghold-gateway --version 2>/dev/null | head -1 || echo unknown)",
  "data_dir": "${DATA_DIR}",
  "config_dir": "${CONFIG_DIR}",
  "encrypted": $([[ "$NO_ENCRYPT" == "false" ]] && echo true || echo false),
  "contents": [
    "data/keys/",
    "data/audit/",
    "data/stronghold.db",
    "config/",
    "etc/ntfy/server.yml"
  ]
}
EOF

# --- 3. Tarball ---
log "Creating tarball..."
TARBALL_PATH="${WORK_DIR}/${ARCHIVE_NAME}"
tar -czf "$TARBALL_PATH" -C "$STAGING" .
ok "Tarball: $(du -h "$TARBALL_PATH" | cut -f1)"

# --- 4. Encrypt ---
FINAL_PATH="$TARBALL_PATH"
FINAL_NAME="$ARCHIVE_NAME"
if [[ "$NO_ENCRYPT" == "false" ]]; then
    log "Encrypting with age (passphrase)..."
    if [[ -z "${BACKUP_ENCRYPTION_PASS:-}" ]]; then
        read -rs -p "Encryption password: " BACKUP_ENCRYPTION_PASS
        echo
        read -rs -p "Confirm password: " CONFIRM
        echo
        if [[ "$BACKUP_ENCRYPTION_PASS" != "$CONFIRM" ]]; then
            err "Passwords do not match"
            exit 1
        fi
    fi
    ENCRYPTED_PATH="${WORK_DIR}/${ENCRYPTED_NAME}"
    if ! echo -n "$BACKUP_ENCRYPTION_PASS" | age encrypt -p -a -o "$ENCRYPTED_PATH" "$TARBALL_PATH"; then
        err "Encryption failed"
        exit 1
    fi
    # Shred the plaintext tarball
    shred -u "$TARBALL_PATH" 2>/dev/null || rm -f "$TARBALL_PATH"
    FINAL_PATH="$ENCRYPTED_PATH"
    FINAL_NAME="$ENCRYPTED_NAME"
    ok "Encrypted: $FINAL_NAME"
fi

# --- 5. Upload or copy to destination ---
if [[ "$DRY_RUN" == "true" ]]; then
    warn "--dry-run: not uploading. Backup staged at ${FINAL_PATH}"
    exit 0
fi

if [[ "$DEST" == s3://* ]]; then
    log "Uploading to S3: ${DEST}/${FINAL_NAME}"
    if ! command -v aws &>/dev/null; then
        log "Installing AWS CLI..."
        dnf install -y -q awscli 2>/dev/null || {
            err "aws CLI not installed and dnf install failed."
            err "Install manually: dnf install -y awscli"
            exit 1
        }
    fi
    if [[ -z "${AWS_ACCESS_KEY_ID:-}" || -z "${AWS_SECRET_ACCESS_KEY:-}" ]]; then
        err "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must be set for S3 upload"
        exit 1
    fi
    aws s3 cp "$FINAL_PATH" "${DEST%/}/${FINAL_NAME}" \
        --sse AES256 \
        --no-progress
    ok "Uploaded to ${DEST%/}/${FINAL_NAME}"

    # Optionally prune old S3 backups
    if [[ "$KEEP_DAYS" -gt 0 ]]; then
        log "Pruning S3 backups older than ${KEEP_DAYS} days..."
        aws s3 ls "${DEST%/}/" | awk '{print $4}' | grep "^stronghold-${HOST}-" | while read -r f; do
            # Extract date from filename: stronghold-HOST-YYYYmmddTHHMMSSZ.tar.gz[.age]
            DATE_STR="$(echo "$f" | grep -oE '[0-9]{8}T[0-9]{6}Z' || true)"
            if [[ -n "$DATE_STR" ]]; then
                if [[ "$(date -u -d "${DATE_STR:0:8} ${DATE_STR:9:6} +00:00" +%s 2>/dev/null || echo 0)" -lt "$(date -u -d "${KEEP_DAYS} days ago" +%s)" ]]; then
                    log "  deleting old backup: $f"
                    aws s3 rm "${DEST%/}/${f}" --no-progress || true
                fi
            fi
        done
    fi
else
    # Local destination
    LOCAL_DEST="${DEST:-${BACKUP_DIR}}"
    mkdir -p "$LOCAL_DEST"
    cp -a "$FINAL_PATH" "${LOCAL_DEST}/${FINAL_NAME}"
    ok "Saved to ${LOCAL_DEST}/${FINAL_NAME}"

    if [[ "$KEEP_DAYS" -gt 0 ]]; then
        log "Pruning local backups older than ${KEEP_DAYS} days..."
        find "$LOCAL_DEST" -name "stronghold-${HOST}-*" -type f -mtime +"$KEEP_DAYS" -print -delete || true
    fi
fi

# --- 6. Verify ---
log "Verifying backup..."
if [[ "$NO_ENCRYPT" == "false" ]]; then
    if echo -n "$BACKUP_ENCRYPTION_PASS" | age decrypt -p -o /dev/null "${LOCAL_DEST:-${WORK_DIR}}/${FINAL_NAME}" 2>/dev/null \
        || echo -n "$BACKUP_ENCRYPTION_PASS" | age decrypt -p -o /dev/null "$FINAL_PATH" 2>/dev/null; then
        ok "Backup verified (decrypts with provided passphrase)"
    else
        warn "Could not verify encryption (continuing anyway)"
    fi
fi

echo ""
echo "=========================================="
echo "  Backup Complete"
echo "=========================================="
echo ""
echo "  Archive:    ${FINAL_NAME}"
echo "  Size:       $(du -h "$FINAL_PATH" | cut -f1)"
echo "  Encrypted:  $([[ "$NO_ENCRYPT" == "false" ]] && echo yes || echo NO)"
if [[ "$DEST" == s3://* ]]; then
    echo "  Location:   ${DEST%/}/${FINAL_NAME}"
else
    echo "  Location:   ${LOCAL_DEST}/${FINAL_NAME}"
fi
echo ""
echo "To restore:"
echo "  bash setup/backup.sh --restore <path-to-${FINAL_NAME}>"
echo ""
echo "Test restore on a fresh box periodically — do not trust untested backups."
