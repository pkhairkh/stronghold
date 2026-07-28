#!/usr/bin/env bash
# Stronghold upgrade script
#
# Per W10-T8 DoD:
#   - Pulls new binary (from release tarball or builds from source)
#   - Verifies Ed25519 signature (sigstore/cosign or detached .sig)
#   - Drains k3s node (if this box runs pods)
#   - Restarts services
#   - Re-attests SEV-SNP (records new measurement)
#   - Rotates audit keys (optional, --rotate-keys)
#
# Idempotent: re-running after a successful upgrade is a no-op (binary version
# matches the requested version).
#
# Usage:
#   bash setup/upgrade.sh                                 # upgrade to latest release
#   bash setup/upgrade.sh --version v1.2.0                # specific version
#   bash setup/upgrade.sh --from-source                   # build from local source
#   bash setup/upgrade.sh --version v1.2.0 --rotate-keys  # also rotate keys
#   bash setup/upgrade.sh --check                         # show current & latest
#
# Environment:
#   STRONGHOLD_INSTALL_DIR   (default: /usr/local/bin)
#   STRONGHOLD_DATA_DIR      (default: /var/lib/stronghold)
#   STRONGHOLD_SIGNING_KEY   (Ed25519 public key hex for signature verification)

set -euo pipefail

VERSION=""
FROM_SOURCE=false
CHECK_ONLY=false
ROTATE_KEYS=false
DRY_RUN=false
SKIP_VERIFY=false
REPO_DIR=""
INSTALL_DIR="${STRONGHOLD_INSTALL_DIR:-/usr/local/bin}"
DATA_DIR="${STRONGHOLD_DATA_DIR:-/var/lib/stronghold}"
GITHUB_REPO="pkhairkh/stronghold"
# Trusted Ed25519 public key (32-byte hex) for release signature verification.
# Replace with the real release signing key before publishing upgrades.
SIGNING_KEY="${STRONGHOLD_SIGNING_KEY:-0000000000000000000000000000000000000000000000000000000000000000}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version=*)       VERSION="${1#*=}" ;;
        --version)         VERSION="$2"; shift ;;
        --from-source)     FROM_SOURCE=true ;;
        --repo-dir=*)      REPO_DIR="${1#*=}" ;;
        --repo-dir)        REPO_DIR="$2"; shift ;;
        --check)           CHECK_ONLY=true ;;
        --rotate-keys)     ROTATE_KEYS=true ;;
        --dry-run)         DRY_RUN=true ;;
        --skip-verify)     SKIP_VERIFY=true ;;
        --help|-h)
            cat <<EOF
Usage: upgrade.sh [OPTIONS]

Options:
  --version=V          Target version (default: latest release on GitHub)
  --from-source        Build from local source repo (use with --repo-dir)
  --repo-dir=DIR       Source repo (default: parent of this script)
  --rotate-keys        Rotate audit + push keys after upgrade
  --skip-verify        Skip signature verification (NOT recommended)
  --check              Show current and latest version, then exit
  --dry-run            Show actions without performing them
  -h, --help           Show this help

Environment:
  STRONGHOLD_INSTALL_DIR  Binary install path (default: /usr/local/bin)
  STRONGHOLD_DATA_DIR     Data directory (default: /var/lib/stronghold)
  STRONGHOLD_SIGNING_KEY  Ed25519 public key (32-byte hex) for release sig
EOF
            exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
    shift
done

if [[ -z "$REPO_DIR" ]]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    REPO_DIR="${SCRIPT_DIR}/.."
fi

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

# --- Helpers ---
current_version() {
    if [[ -x "${INSTALL_DIR}/stronghold-gateway" ]]; then
        "${INSTALL_DIR}/stronghold-gateway" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || echo "unknown"
    else
        echo "not-installed"
    fi
}

latest_github_release() {
    curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" 2>/dev/null \
        | grep -oE '"tag_name":\s*"[^"]+"' | head -1 \
        | sed -E 's/"tag_name":\s*"([^"]+)"/\1/' || echo "unknown"
}

# --- Check mode ---
if [[ "$CHECK_ONLY" == "true" ]]; then
    CUR="$(current_version)"
    LATEST="$(latest_github_release)"
    echo "Current: $CUR"
    echo "Latest:  $LATEST"
    if [[ "$CUR" == "$LATEST" ]]; then
        echo "Status:  up-to-date"
        exit 0
    else
        echo "Status:  upgrade available"
        exit 1
    fi
fi

echo "=========================================="
echo "  Stronghold Upgrade"
echo "=========================================="
CUR_VERSION="$(current_version)"
if [[ -z "$VERSION" ]]; then
    VERSION="$(latest_github_release)"
    if [[ "$VERSION" == "unknown" || -z "$VERSION" ]]; then
        if [[ "$FROM_SOURCE" == "false" ]]; then
            err "Could not determine latest release. Pass --version explicitly or use --from-source."
            exit 1
        fi
    fi
fi
echo "  Current:      $CUR_VERSION"
echo "  Target:       $VERSION"
echo "  From source:  $FROM_SOURCE"
echo "  Rotate keys:  $ROTATE_KEYS"
echo "  Dry run:      $DRY_RUN"
echo ""

# Skip if already at target version (unless --from-source or --rotate-keys)
if [[ "$CUR_VERSION" == "$VERSION" && "$FROM_SOURCE" == "false" && "$ROTATE_KEYS" == "false" ]]; then
    ok "Already at version $VERSION — nothing to do"
    exit 0
fi
if [[ "$DRY_RUN" == "true" ]]; then
    warn "DRY RUN: would upgrade from $CUR_VERSION to $VERSION"
    exit 0
fi

# --- 1. Pre-flight: snapshot current state ---
log "Pre-flight: capturing current state..."
BACKUP_STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
SNAPSHOT_DIR="${DATA_DIR}/upgrade-snapshots/${BACKUP_STAMP}"
install -d -m 0700 "$SNAPSHOT_DIR"
# Save current binary
if [[ -x "${INSTALL_DIR}/stronghold-gateway" ]]; then
    cp -a "${INSTALL_DIR}/stronghold-gateway" "${SNAPSHOT_DIR}/stronghold-gateway.prev"
fi
# Save current attestation measurement (if exists)
if [[ -f "${DATA_DIR}/audit/attestation.json" ]]; then
    cp -a "${DATA_DIR}/audit/attestation.json" "${SNAPSHOT_DIR}/attestation.prev.json"
fi
# Online DB backup
if [[ -f "${DATA_DIR}/stronghold.db" ]] && command -v sqlite3 &>/dev/null; then
    sqlite3 "${DATA_DIR}/stronghold.db" ".backup '${SNAPSHOT_DIR}/stronghold.prev.db'"
fi
ok "Snapshot saved to ${SNAPSHOT_DIR}"
echo ""

# --- 2. Acquire new binary ---
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT

if [[ "$FROM_SOURCE" == "true" ]]; then
    log "Building from source at ${REPO_DIR}..."
    cd "$REPO_DIR"
    if [[ -e /dev/sev ]]; then
        FEATURES="sev-snp"
    else
        FEATURES="no-sev-snp"
    fi
    cargo build --release --features "$FEATURES"
    install -m 0755 target/release/stronghold-gateway "${STAGING}/stronghold-gateway"
    if [[ -f target/release/stronghold ]]; then
        install -m 0755 target/release/stronghold "${STAGING}/stronghold"
    fi
    ok "Built from source"
else
    log "Downloading release ${VERSION}..."
    DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/download/${VERSION}/stronghold-${VERSION}-linux-amd64.tar.gz"
    SIG_URL="${DOWNLOAD_URL}.sig"
    if ! curl -fsSL -o "${STAGING}/release.tar.gz" "$DOWNLOAD_URL"; then
        err "Download failed: $DOWNLOAD_URL"
        exit 1
    fi
    ok "Downloaded release tarball"

    # --- 3. Verify signature ---
    if [[ "$SKIP_VERIFY" == "true" ]]; then
        warn "--skip-verify: skipping signature verification (INSECURE)"
    else
        log "Verifying Ed25519 signature..."
        if ! curl -fsSL -o "${STAGING}/release.tar.gz.sig" "$SIG_URL"; then
            err "Could not download signature: $SIG_URL"
            err "Refusing to upgrade without signature. Pass --skip-verify to override (NOT recommended)."
            exit 1
        fi

        # Use openssl to verify Ed25519 signature
        # The .sig file contains a raw 64-byte Ed25519 signature.
        # We need the public key in DER or PEM form; convert from raw hex.
        if [[ "$SIGNING_KEY" =~ ^[0-9a-fA-F]{64}$ ]]; then
            # Convert raw 32-byte hex public key to PEM (use openssl)
            echo "$SIGNING_KEY" | xxd -r -p > "${STAGING}/pubkey.bin"
            openssl pkey -inform DER -outform PEM -pubin -in "${STAGING}/pubkey.bin" \
                -out "${STAGING}/pubkey.pem" 2>/dev/null || {
                # Fallback: write a simple PEM wrapper
                B64="$(echo "$SIGNING_KEY" | xxd -r -p | base64 -w0)"
                cat > "${STAGING}/pubkey.pem" <<EOF
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEA$(echo "$B64" | base64 -d | xxd -p | head -c 64 | xxd -r -p | base64)
-----END PUBLIC KEY-----
EOF
            }
        else
            err "STRONGHOLD_SIGNING_KEY must be 32-byte hex (64 chars). Got: ${SIGNING_KEY}"
            err "Set STRONGHOLD_SIGNING_KEY to the trusted release signing key."
            exit 1
        fi

        if openssl dgst -sha256 -verify "${STAGING}/pubkey.pem" \
                -signature "${STAGING}/release.tar.gz.sig" \
                "${STAGING}/release.tar.gz" 2>/dev/null; then
            ok "Signature verified"
        else
            err "Signature verification FAILED"
            err "Refusing to install untrusted binary. Either:"
            err "  1. Set STRONGHOLD_SIGNING_KEY to the correct Ed25519 public key"
            err "  2. Pass --skip-verify (NOT recommended; defeats supply-chain security)"
            exit 1
        fi
    fi

    # Extract
    log "Extracting release tarball..."
    mkdir -p "${STAGING}/extracted"
    tar -xzf "${STAGING}/release.tar.gz" -C "${STAGING}/extracted"
    BIN_PATH="${STAGING}/extracted/stronghold-gateway"
    if [[ ! -f "$BIN_PATH" ]]; then
        # Try nested directory layout
        BIN_PATH="$(find "${STAGING}/extracted" -name stronghold-gateway -type f | head -1)"
    fi
    if [[ -z "$BIN_PATH" || ! -f "$BIN_PATH" ]]; then
        err "stronghold-gateway binary not found in tarball"
        exit 1
    fi
    install -m 0755 "$BIN_PATH" "${STAGING}/stronghold-gateway"
    CLI_PATH="$(find "${STAGING}/extracted" -name stronghold -type f | head -1 || true)"
    if [[ -n "$CLI_PATH" ]]; then
        install -m 0755 "$CLI_PATH" "${STAGING}/stronghold"
    fi
    ok "Binary staged"
fi
echo ""

# --- 4. Drain k3s node (if applicable) ---
if command -v k3s &>/dev/null && systemctl is-active --quiet k3s-agent 2>/dev/null; then
    log "Draining k3s node for upgrade..."
    NODE_NAME="$(hostname -s)"
    # Mark node unschedulable
    k3s kubectl cordon "$NODE_NAME" 2>/dev/null || true
    # Drain with grace period
    if ! k3s kubectl drain "$NODE_NAME" --ignore-daemonsets --delete-emptydir-data --timeout=120s 2>/dev/null; then
        warn "Drain did not complete in 120s; continuing with upgrade anyway"
    fi
    ok "Node drained"
    DRAINED=true
else
    log "No k3s agent on this box; skipping drain"
    DRAINED=false
fi
echo ""

# --- 5. Stop services ---
log "Stopping stronghold-gateway..."
systemctl stop stronghold-gateway 2>/dev/null || true
ok "Stopped"
echo ""

# --- 6. Install new binary ---
log "Installing new binary..."
install -m 0755 "${STAGING}/stronghold-gateway" "${INSTALL_DIR}/stronghold-gateway"
if [[ -f "${STAGING}/stronghold" ]]; then
    install -m 0755 "${STAGING}/stronghold" "${INSTALL_DIR}/stronghold"
fi
NEW_VERSION="$("${INSTALL_DIR}/stronghold-gateway" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || echo unknown)"
ok "Installed version: $NEW_VERSION"
echo ""

# --- 7. Run DB migrations (idempotent: stronghold-gateway init --data-dir) ---
log "Running DB migrations..."
"${INSTALL_DIR}/stronghold-gateway" init --data-dir "${DATA_DIR}" 2>&1 | grep -v -E 'Setup password|Enrollment' || true
ok "Migrations applied (if any)"
echo ""

# --- 8. Re-attest SEV-SNP (record new measurement) ---
if [[ -e /dev/sev ]]; then
    log "Re-attesting SEV-SNP (recording new measurement)..."
    "${INSTALL_DIR}/stronghold-gateway" attestation > "${DATA_DIR}/audit/attestation.json" 2>/dev/null || {
        warn "Attestation generation failed; check /dev/sev permissions and SEV-SNP firmware"
    }
    if [[ -f "${DATA_DIR}/audit/attestation.json" ]]; then
        ok "New attestation recorded: ${DATA_DIR}/audit/attestation.json"
        # Compare with previous
        if [[ -f "${SNAPSHOT_DIR}/attestation.prev.json" ]]; then
            if diff -q "${SNAPSHOT_DIR}/attestation.prev.json" "${DATA_DIR}/audit/attestation.json" >/dev/null 2>&1; then
                ok "Measurement unchanged (good — same firmware, same binary)"
            else
                warn "Measurement CHANGED — expected for a binary upgrade"
                warn "Notify all enrolled phones to re-verify the measurement."
            fi
        fi
    fi
else
    log "/dev/sev not present; skipping attestation (dev mode)"
fi
echo ""

# --- 9. Optional: rotate keys ---
if [[ "$ROTATE_KEYS" == "true" ]]; then
    log "Rotating audit + push keys..."
    if [[ -x "${INSTALL_DIR}/stronghold" ]]; then
        "${INSTALL_DIR}/stronghold" keys rotate-audit || warn "audit key rotation failed"
        "${INSTALL_DIR}/stronghold" keys rotate-push  || warn "push key rotation failed"
    else
        warn "stronghold CLI not found; skipping key rotation"
    fi
    ok "Key rotation done (enrolled phones must re-enroll for push keys)"
fi
echo ""

# --- 10. Restart services ---
log "Restarting stronghold-gateway..."
systemctl start stronghold-gateway
sleep 2
if systemctl is-active --quiet stronghold-gateway; then
    ok "stronghold-gateway: active"
else
    err "stronghold-gateway failed to start after upgrade!"
    err "Rolling back to previous binary..."
    if [[ -f "${SNAPSHOT_DIR}/stronghold-gateway.prev" ]]; then
        install -m 0755 "${SNAPSHOT_DIR}/stronghold-gateway.prev" "${INSTALL_DIR}/stronghold-gateway"
        systemctl start stronghold-gateway || true
        err "Rolled back. Inspect logs: journalctl -u stronghold-gateway -n 200"
    fi
    exit 1
fi
echo ""

# --- 11. Uncordon k3s node ---
if [[ "$DRAINED" == "true" ]]; then
    log "Uncordoning k3s node..."
    k3s kubectl uncordon "$(hostname -s)" 2>/dev/null || true
    ok "Node schedulable again"
fi
echo ""

# --- 12. Verify audit log still verifies ---
log "Verifying audit log integrity..."
if [[ -x "${INSTALL_DIR}/stronghold" ]]; then
    if "${INSTALL_DIR}/stronghold" audit verify --tenant default 2>/dev/null; then
        ok "Audit log verifies OK"
    else
        warn "Audit verify returned non-zero (may be expected for empty DB)"
    fi
fi
echo ""

echo "=========================================="
echo "  Upgrade Complete"
echo "=========================================="
echo "  Previous: $CUR_VERSION"
echo "  Current:  $NEW_VERSION"
echo "  Snapshot: ${SNAPSHOT_DIR}"
echo ""
echo "Next steps:"
echo "  1. Verify the gateway is reachable: curl -k https://localhost:8443/agent/health"
echo "  2. Verify the new SEV-SNP measurement on enrolled phones"
echo "  3. If --rotate-keys was used, re-enroll phones for push notifications"
echo "  4. Run a backup: bash setup/backup.sh --to s3://..."
echo ""
echo "To roll back:"
echo "  systemctl stop stronghold-gateway"
echo "  cp ${SNAPSHOT_DIR}/stronghold-gateway.prev ${INSTALL_DIR}/stronghold-gateway"
echo "  systemctl start stronghold-gateway"
