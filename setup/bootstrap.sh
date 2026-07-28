#!/usr/bin/env bash
# Stronghold Control Plane Bootstrap Script
#
# Installs and configures the Stronghold gateway on a fresh Vultr box.
# Idempotent: safe to re-run, will skip work already done.
#
# Supports:
#   - Rocky Linux 9 and 10
#   - AMD SEV-SNP support (or run with --dev for development)
#
# Usage:
#   bash setup/bootstrap.sh                  # production (requires /dev/sev)
#   bash setup/bootstrap.sh --dev            # skip SEV-SNP check
#   bash setup/bootstrap.sh --dev --build-only   # just build, don't install/start
#
# Environment overrides:
#   STRONGHOLD_DATA_DIR     (default: /var/lib/stronghold)
#   STRONGHOLD_INSTALL_DIR  (default: /usr/local/bin)
#   STRONGHOLD_CONFIG_DIR   (default: /etc/stronghold)
#   STRONGHOLD_REPO_DIR     (default: parent of script directory)
#   STRONGHOLD_BUILD_FEATURES (default: auto-detect from /dev/sev)
#
# Exit codes:
#   0  Success
#   1  Generic failure
#   2  Missing requirement (root, OS, SEV-SNP)
#   3  Build failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEV_MODE=false
BUILD_ONLY=false
DATA_DIR="${STRONGHOLD_DATA_DIR:-/var/lib/stronghold}"
INSTALL_DIR="${STRONGHOLD_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${STRONGHOLD_CONFIG_DIR:-/etc/stronghold}"
REPO_DIR="${STRONGHOLD_REPO_DIR:-${SCRIPT_DIR}/..}"
BUILD_FEATURES_OVERRIDE=""

# Parse args
for arg in "$@"; do
    case "$arg" in
        --dev)                 DEV_MODE=true ;;
        --build-only)          BUILD_ONLY=true ;;
        --data-dir=*)          DATA_DIR="${arg#*=}" ;;
        --install-dir=*)       INSTALL_DIR="${arg#*=}" ;;
        --config-dir=*)        CONFIG_DIR="${arg#*=}" ;;
        --repo-dir=*)          REPO_DIR="${arg#*=}" ;;
        --features=*)          BUILD_FEATURES_OVERRIDE="${arg#*=}" ;;
        --help|-h)
            cat <<EOF
Usage: bootstrap.sh [OPTIONS]

Options:
  --dev               Skip SEV-SNP check (development only)
  --build-only        Build and install binary; do not start services
  --data-dir=DIR      Data directory (default: /var/lib/stronghold)
  --install-dir=DIR   Binary install path (default: /usr/local/bin)
  --config-dir=DIR    Config directory (default: /etc/stronghold)
  --repo-dir=DIR      Source repo (default: parent of script)
  --features=F        Cargo feature set (default: sev-snp or no-sev-snp)
  -h, --help          Show this help

Environment:
  STRONGHOLD_DATA_DIR, STRONGHOLD_INSTALL_DIR, STRONGHOLD_CONFIG_DIR,
  STRONGHOLD_REPO_DIR, STRONGHOLD_BUILD_FEATURES
EOF
            exit 0
            ;;
        *)
            echo "ERROR: Unknown option: $arg" >&2
            exit 1
            ;;
    esac
done

# Color helpers (only when stdout is a TTY)
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

echo "=========================================="
echo "  Stronghold Control Plane Bootstrap"
echo "=========================================="
echo ""

# --- Check root ---
if [[ $EUID -ne 0 ]]; then
    err "Run as root (use sudo)"
    exit 2
fi

# --- Check OS (Rocky 9 or 10) ---
if [[ -f /etc/rocky-release ]]; then
    ROCKY_VER=$(grep -oE 'VERSION="[0-9]+' /etc/os-release | head -1 | grep -oE '[0-9]+')
    log "Detected Rocky Linux ${ROCKY_VER}"
    if [[ "$ROCKY_VER" != "9" && "$ROCKY_VER" != "10" ]]; then
        warn "Rocky Linux ${ROCKY_VER} is untested. Supported: 9, 10. Continuing anyway."
    fi
else
    warn "Not Rocky Linux. Detected: $(grep '^PRETTY_NAME' /etc/os-release 2>/dev/null || echo unknown)"
    warn "Continuing anyway — script targets RHEL-family (dnf, systemd, firewalld)."
fi
echo ""

# --- Check SEV-SNP ---
log "Checking SEV-SNP availability..."
SEV_SNP_AVAILABLE=false
if [[ -e /dev/sev ]]; then
    ok "/dev/sev present — SEV-SNP capable"
    SEV_SNP_AVAILABLE=true
else
    warn "/dev/sev not found"
    if [[ "$DEV_MODE" == "false" ]]; then
        err "SEV-SNP is required for production. Either:"
        err "  1. Provision a SEV-SNP-capable Vultr plan"
        err "  2. Re-run with --dev for development (NOT for production!)"
        exit 2
    fi
    ok "Running in --dev mode, continuing without SEV-SNP"
fi
echo ""

# --- Install system dependencies ---
log "Installing system dependencies (idempotent)..."
dnf install -y -q \
    git curl wget jq \
    gcc gcc-c++ make cmake \
    openssl openssl-devel \
    sqlite sqlite-devel \
    podman \
    policycoreutils-python-utils \
    firewalld \
    perl \
    2>/dev/null
# Ensure firewalld is enabled/running (non-fatal if it fails in container)
systemctl enable --now firewalld 2>/dev/null || true
ok "Dependencies installed"
echo ""

# --- Install Rust (idempotent) ---
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin/cargo"
if ! command -v cargo &>/dev/null && [[ ! -x "$CARGO_BIN" ]]; then
    log "Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
    ok "Rust installed: $(rustc --version)"
else
    # Find cargo even if not in PATH
    if ! command -v cargo &>/dev/null; then
        # shellcheck disable=SC1091
        source "$HOME/.cargo/env" 2>/dev/null || true
    fi
    ok "Rust already installed: $(rustc --version)"
fi
echo ""

# --- Build Stronghold ---
if [[ -n "$BUILD_FEATURES_OVERRIDE" ]]; then
    BUILD_FEATURES="$BUILD_FEATURES_OVERRIDE"
elif [[ "$SEV_SNP_AVAILABLE" == "true" ]]; then
    BUILD_FEATURES="sev-snp"
else
    BUILD_FEATURES="no-sev-snp"
fi

log "Building Stronghold (features: ${BUILD_FEATURES})..."
if [[ ! -f "${REPO_DIR}/Cargo.toml" ]]; then
    err "Source not found at ${REPO_DIR}/Cargo.toml"
    err "Pass --repo-dir=/path/to/stronghold or set STRONGHOLD_REPO_DIR"
    exit 1
fi

cd "$REPO_DIR"
# Touching a stamp file would over-engineer this; cargo is incremental itself.
if ! cargo build --release --features "$BUILD_FEATURES"; then
    err "Build failed"
    exit 3
fi

# Install binaries (idempotent: copy is always safe)
install -m 0755 target/release/stronghold-gateway "${INSTALL_DIR}/stronghold-gateway"
if [[ -f target/release/stronghold ]]; then
    install -m 0755 target/release/stronghold "${INSTALL_DIR}/stronghold"
fi
ok "Binaries installed to ${INSTALL_DIR}/"
echo ""

# --- Create directories (idempotent) ---
log "Creating directories..."
install -d -m 0750 -o root -g root "${DATA_DIR}"
install -d -m 0700 -o root -g root "${DATA_DIR}/keys"
install -d -m 0700 -o root -g root "${DATA_DIR}/audit"
install -d -m 0750 -o root -g root "${CONFIG_DIR}"
ok "Data dir:   ${DATA_DIR}"
ok "Config dir: ${CONFIG_DIR}"
echo ""

# --- Initialize Stronghold (idempotent: skips if keys already present) ---
INIT_TO_RUN=false
if [[ ! -f "${DATA_DIR}/stronghold.db" ]]; then
    INIT_TO_RUN=true
fi
if [[ ! -f "${DATA_DIR}/keys/ed25519_secret.key" ]]; then
    INIT_TO_RUN=true
fi

SETUP_PASSWORD=""
if [[ "$INIT_TO_RUN" == "true" ]]; then
    log "Initializing Stronghold (first run)..."
    # Capture stdout to find the setup password; stderr goes to console
    INIT_LOG="$(mktemp)"
    if "${INSTALL_DIR}/stronghold-gateway" init --data-dir "${DATA_DIR}" >"$INIT_LOG" 2>&1; then
        ok "Initialized"
        SETUP_PASSWORD="$(grep -oE '^\s+[A-Za-z0-9]{20,}$' "$INIT_LOG" | head -1 | tr -d '[:space:]')"
    else
        err "stronghold-gateway init failed:"
        cat "$INIT_LOG" >&2
        rm -f "$INIT_LOG"
        exit 1
    fi
    rm -f "$INIT_LOG"
else
    ok "Stronghold already initialized (DB + keys present), skipping init"
fi
echo ""

# --- Generate self-signed TLS cert (idempotent: skip if exists) ---
TLS_CERT="${CONFIG_DIR}/tls.crt"
TLS_KEY="${CONFIG_DIR}/tls.key"
if [[ ! -f "$TLS_CERT" || ! -f "$TLS_KEY" ]]; then
    log "Generating self-signed TLS certificate (dev)..."
    install -d -m 0700 -o root -g root "${CONFIG_DIR}"
    BOX_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
    BOX_HOST="$(hostname -f 2>/dev/null || hostname)"
    openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
        -keyout "$TLS_KEY" -out "$TLS_CERT" \
        -subj "/CN=${BOX_HOST}" \
        -addext "subjectAltName=DNS:${BOX_HOST},DNS:localhost,IP:${BOX_IP:-127.0.0.1}" \
        2>/dev/null
    chmod 600 "$TLS_KEY"
    chmod 644 "$TLS_CERT"
    ok "TLS cert: ${TLS_CERT}"
else
    ok "TLS cert already present, skipping"
fi
echo ""

if [[ "$BUILD_ONLY" == "true" ]]; then
    ok "--build-only: skipping service installation and startup"
    exit 0
fi

# --- Install ntfy (idempotent) ---
if ! command -v ntfy &>/dev/null; then
    log "Installing ntfy..."
    NTFY_RPM_URL="https://github.com/binwiederhier/ntfy/releases/download/v2.11.0/ntfy_2.11.0_linux_amd64.rpm"
    if dnf install -y -q "$NTFY_RPM_URL"; then
        ok "ntfy installed"
    else
        warn "ntfy install failed (network?). Continuing — install manually if push is needed."
    fi
else
    ok "ntfy already installed: $(ntfy --version 2>&1 | head -1)"
fi

# Deploy ntfy config (idempotent: always overwrite to pick up changes)
install -d -m 0750 -o ntfy -g ntfy /etc/ntfy 2>/dev/null || install -d -m 0750 /etc/ntfy
if [[ -f "${SCRIPT_DIR}/ntfy.yml" ]]; then
    install -m 0640 -o ntfy -g ntfy "${SCRIPT_DIR}/ntfy.yml" /etc/ntfy/server.yml 2>/dev/null \
        || install -m 0640 "${SCRIPT_DIR}/ntfy.yml" /etc/ntfy/server.yml
    ok "ntfy config deployed to /etc/ntfy/server.yml"
fi
install -d -m 0750 -o ntfy -g ntfy /var/lib/ntfy 2>/dev/null || install -d -m 0750 /var/lib/ntfy
echo ""

# --- Install systemd units (idempotent: copy + reload) ---
log "Installing systemd units..."
install -m 0644 "${SCRIPT_DIR}/systemd/stronghold-gateway.service" /etc/systemd/system/stronghold-gateway.service
install -m 0644 "${SCRIPT_DIR}/systemd/ntfy.service"               /etc/systemd/system/ntfy.service

# Render the gateway unit with real paths if non-default
if [[ "${DATA_DIR}" != "/var/lib/stronghold" || "${CONFIG_DIR}" != "/etc/stronghold" ]]; then
    sed -i \
        -e "s|Environment=STRONGHOLD_DATA_DIR=/var/lib/stronghold|Environment=STRONGHOLD_DATA_DIR=${DATA_DIR}|g" \
        -e "s|Environment=STRONGHOLD_CONFIG=/etc/stronghold/config.toml|Environment=STRONGHOLD_CONFIG=${CONFIG_DIR}/config.toml|g" \
        -e "s|ReadWritePaths=.*|ReadWritePaths=${DATA_DIR} ${CONFIG_DIR}|g" \
        /etc/systemd/system/stronghold-gateway.service
fi

systemctl daemon-reload
ok "systemd units installed and reloaded"
echo ""

# --- Configure firewall (best-effort; idempotent) ---
log "Configuring firewall..."
if command -v firewall-cmd &>/dev/null && systemctl is-active --quiet firewalld; then
    # 8443 gateway (public), 8090 ntfy (public) — phone needs both
    for p in 8443/tcp 8090/tcp; do
        firewall-cmd --permanent --add-port="$p" 2>/dev/null || true
    done
    firewall-cmd --reload 2>/dev/null || true
    ok "Ports 8443 (gateway) and 8090 (ntfy) opened"
else
    warn "firewalld not active — see setup/firewall.sh for full rules"
fi
echo ""

# --- Enable and (re)start services (idempotent) ---
log "Enabling and starting services..."
systemctl enable stronghold-gateway.service ntfy.service 2>/dev/null || true

# Start ntfy first (gateway depends on it)
systemctl restart ntfy.service
sleep 1
systemctl restart stronghold-gateway.service

# Verify
sleep 2
GW_STATE="$(systemctl is-active stronghold-gateway.service || true)"
NTFY_STATE="$(systemctl is-active ntfy.service || true)"
if [[ "$GW_STATE" == "active" ]]; then
    ok "stronghold-gateway: active"
else
    warn "stronghold-gateway: ${GW_STATE} (check: journalctl -u stronghold-gateway -n 50)"
fi
if [[ "$NTFY_STATE" == "active" ]]; then
    ok "ntfy: active"
else
    warn "ntfy: ${NTFY_STATE} (check: journalctl -u ntfy -n 50)"
fi
echo ""

# --- Print summary ---
BOX_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
BOX_HOST="$(hostname -f 2>/dev/null || hostname)"

echo "=========================================="
echo "  Stronghold Installed"
echo "=========================================="
echo ""
if [[ -n "$SETUP_PASSWORD" ]]; then
    echo "Setup password (save this — it will NOT be shown again):"
    echo "  ${SETUP_PASSWORD}"
    echo ""
fi
echo "Gateway URL: https://${BOX_IP:-localhost}:8443"
echo "ntfy URL:    http://${BOX_IP:-localhost}:8090"
echo "Host (fqdn): ${BOX_HOST}"
echo ""
if [[ "$SEV_SNP_AVAILABLE" == "true" ]]; then
    echo "SEV-SNP:    ACTIVE (/dev/sev present)"
    echo "Measurement: see ${DATA_DIR}/audit/attestation.json after first request"
else
    echo "SEV-SNP:    NOT ACTIVE (dev mode)"
fi
echo ""
echo "Data dir:    ${DATA_DIR}"
echo "Config dir:  ${CONFIG_DIR}"
echo "Binaries:    ${INSTALL_DIR}/{stronghold-gateway,stronghold}"
echo ""
echo "Next steps:"
echo "  1. Open the Gateway URL in your phone browser"
if [[ -n "$SETUP_PASSWORD" ]]; then
    echo "  2. Enter the setup password shown above"
else
    echo "  2. Retrieve the setup password from the DB or re-init"
fi
echo "  3. Verify the SEV-SNP measurement (if applicable)"
echo "  4. Complete Face ID enrollment"
echo "  5. Mint an agent token: stronghold agent-token mint --tenant <id> --ttl 86400"
echo ""
echo "Useful commands:"
echo "  systemctl status stronghold-gateway"
echo "  journalctl -u stronghold-gateway -f"
echo "  stronghold worker list   # after workers are added"
echo ""
echo "Done."
