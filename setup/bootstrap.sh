#!/usr/bin/env bash
# Stronghold Control Plane Bootstrap Script
#
# Installs and configures the Stronghold gateway on a Vultr box.
# Requires AMD SEV-SNP support (or run with --dev for development).
#
# Usage:
#   curl -sL https://github.com/pkhairkh/stronghold/releases/latest/download/bootstrap.sh | bash
#   bash bootstrap.sh --dev   # skip SEV-SNP check

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEV_MODE=false
DATA_DIR="${STRONGHOLD_DATA_DIR:-/var/lib/stronghold}"
INSTALL_DIR="${STRONGHOLD_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${STRONGHOLD_CONFIG_DIR:-/etc/stronghold}"

# Parse args
for arg in "$@"; do
    case "$arg" in
        --dev) DEV_MODE=true ;;
        --data-dir=*) DATA_DIR="${arg#*=}" ;;
        --help)
            echo "Usage: bootstrap.sh [--dev] [--data-dir=DIR]"
            echo ""
            echo "Options:"
            echo "  --dev              Skip SEV-SNP check (for development)"
            echo "  --data-dir=DIR     Data directory (default: /var/lib/stronghold)"
            exit 0
            ;;
    esac
done

echo "=========================================="
echo "  Stronghold Control Plane Bootstrap"
echo "=========================================="
echo ""

# --- Check OS ---
if [[ ! -f /etc/rocky-release ]]; then
    echo "WARNING: This script is designed for Rocky Linux."
    echo "Detected OS: $(cat /etc/os-release 2>/dev/null | grep '^PRETTY_NAME' || echo 'unknown')"
    echo "Continue anyway? (y/N)"
    read -r response
    [[ "$response" =~ ^[yY]$ ]] || exit 1
fi

# --- Check root ---
if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Run as root (use sudo)"
    exit 1
fi

# --- Check SEV-SNP ---
echo "Checking SEV-SNP availability..."
if [[ -f /dev/sev ]]; then
    echo "  SEV-SNP device detected at /dev/sev"
    SEV_SNP_AVAILABLE=true
else
    echo "  WARNING: /dev/sev not found"
    if [[ "$DEV_MODE" == "false" ]]; then
        echo "  SEV-SNP is required for production."
        echo "  Either:"
        echo "    1. Provision a SEV-SNP-capable Vultr plan"
        echo "    2. Re-run with --dev for development (not for production!)"
        exit 1
    else
        echo "  Running in --dev mode, continuing without SEV-SNP"
        SEV_SNP_AVAILABLE=false
    fi
fi
echo ""

# --- Install dependencies ---
echo "Installing system dependencies..."
dnf install -y -q \
    git curl wget jq \
    gcc gcc-c++ make cmake \
    openssl-devel \
    podman \
    sqlite-devel \
    policycoreutils-python-utils \
    2>/dev/null || true
echo "  Done"
echo ""

# --- Install Rust ---
if ! command -v cargo &>/dev/null; then
    echo "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
    echo "  Done"
else
    echo "Rust already installed: $(rustc --version)"
fi
echo ""

# --- Build Stronghold ---
echo "Building Stronghold..."
BUILD_FEATURES="sev-snp"
if [[ "$SEV_SNP_AVAILABLE" == "false" ]]; then
    BUILD_FEATURES="no-sev-snp"
fi

REPO_DIR="${SCRIPT_DIR}/.."
if [[ -f "${REPO_DIR}/Cargo.toml" ]]; then
    cd "$REPO_DIR"
    cargo build --release --features "$BUILD_FEATURES"
    cp target/release/stronghold-gateway "${INSTALL_DIR}/"
    cp target/release/stronghold "${INSTALL_DIR}/"
    echo "  Binaries installed to ${INSTALL_DIR}/"
else
    echo "  No source found at ${REPO_DIR}"
    echo "  Downloading pre-built binary (stub)..."
    # TODO: download from GitHub releases
    echo "  ERROR: No binary available. Build from source."
    exit 1
fi
echo ""

# --- Create directories ---
echo "Creating directories..."
mkdir -p "${DATA_DIR}/keys"
mkdir -p "${DATA_DIR}/audit"
mkdir -p "${CONFIG_DIR}"
chown -R root:root "${DATA_DIR}" "${CONFIG_DIR}"
chmod 700 "${DATA_DIR}/keys"
echo "  ${DATA_DIR}/"
echo "  ${CONFIG_DIR}/"
echo ""

# --- Initialize Stronghold ---
echo "Initializing Stronghold..."
stronghold-gateway init --data-dir "${DATA_DIR}" 2>&1 | tee /tmp/stronghold-init.log
SETUP_PASSWORD=$(grep -oP '^\s+\K.*' /tmp/stronghold-init.log | head -1)
echo ""

# --- Install ntfy ---
if ! command -v ntfy &>/dev/null; then
    echo "Installing ntfy..."
    dnf install -y -q https://github.com/binwiederhier/ntfy/releases/download/v2.11.0/ntfy_2.11.0_linux_amd64.rpm 2>/dev/null || true
    echo "  Done"
fi
echo ""

# --- Install systemd units ---
echo "Installing systemd units..."
cp "${SCRIPT_DIR}/systemd/stronghold-gateway.service" /etc/systemd/system/
cp "${SCRIPT_DIR}/systemd/ntfy.service" /etc/systemd/system/
systemctl daemon-reload
echo "  Done"
echo ""

# --- Configure firewall ---
echo "Configuring firewall..."
if command -v firewall-cmd &>/dev/null; then
    firewall-cmd --permanent --add-port=8443/tcp  # gateway
    firewall-cmd --permanent --add-port=8090/tcp  # ntfy
    firewall-cmd --reload
    echo "  Ports 8443 (gateway) and 8090 (ntfy) opened"
else
    echo "  firewalld not installed, skipping"
fi
echo ""

# --- Start services ---
echo "Starting services..."
systemctl enable stronghold-gateway ntfy
systemctl start ntfy
systemctl start stronghold-gateway
echo "  Done"
echo ""

# --- Print summary ---
echo "=========================================="
echo "  Stronghold Installed Successfully!"
echo "=========================================="
echo ""
echo "Setup password (save this — it will not be shown again):"
echo "  ${SETUP_PASSWORD}"
echo ""
echo "Gateway URL: https://$(hostname -I | awk '{print $1}'):8443"
echo "ntfy URL:    http://$(hostname -I | awk '{print $1}'):8090"
echo ""
if [[ "$SEV_SNP_AVAILABLE" == "true" ]]; then
    echo "SEV-SNP: ACTIVE"
    echo "Measurement: (see /var/lib/stronghold/keys/measurement.txt)"
else
    echo "SEV-SNP: NOT ACTIVE (running in dev mode)"
fi
echo ""
echo "Next steps:"
echo "  1. Open the Gateway URL in your phone browser"
echo "  2. Enter the setup password"
echo "  3. Verify the SEV-SNP measurement"
echo "  4. Complete Face ID enrollment"
echo "  5. Mint an agent token: stronghold agent-token mint --tenant <id> --ttl 86400"
echo ""
echo "Done!"
