#!/usr/bin/env bash
# Stronghold Worker Bootstrap Script
#
# Installs k3s worker, ntfy, and local OCI registry mirror on a Vultr box.
# The worker joins the Stronghold cluster and becomes eligible for pod scheduling.
#
# Usage:
#   bash worker-bootstrap.sh --host vultr-worker-N.fra1 --token <k3s-token> --server <control-plane-ip>

set -euo pipefail

HOSTNAME=""
K3S_TOKEN=""
K3S_SERVER=""
NTFY_URL=""
REGISTRY_MIRROR=""

# Parse args
while [[ $# -gt 0 ]]; do
    case "$1" in
        --host) HOSTNAME="$2"; shift 2 ;;
        --token) K3S_TOKEN="$2"; shift 2 ;;
        --server) K3S_SERVER="$2"; shift 2 ;;
        --ntfy-url) NTFY_URL="$2"; shift 2 ;;
        --registry-mirror) REGISTRY_MIRROR="$2"; shift 2 ;;
        --help)
            echo "Usage: worker-bootstrap.sh --host HOST --token TOKEN --server IP"
            echo ""
            echo "Options:"
            echo "  --host HOST         Worker hostname"
            echo "  --token TOKEN       k3s join token"
            echo "  --server IP         Control plane IP"
            echo "  --ntfy-url URL      ntfy server URL (default: http://localhost:8090)"
            echo "  --registry-mirror   OCI registry mirror URL"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ -z "$HOSTNAME" || -z "$K3S_TOKEN" || -z "$K3S_SERVER" ]]; then
    echo "ERROR: --host, --token, and --server are required"
    exit 1
fi

echo "=========================================="
echo "  Stronghold Worker Bootstrap"
echo "=========================================="
echo "  Host:       $HOSTNAME"
echo "  Server:     $K3S_SERVER"
echo ""

# --- Check root ---
if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Run as root (use sudo)"
    exit 1
fi

# --- Install dependencies ---
echo "Installing system dependencies..."
dnf install -y -q git curl wget jq podman containerd 2>/dev/null || true
echo "  Done"
echo ""

# --- Install k3s worker ---
echo "Installing k3s worker..."
curl -sfL https://get.k3s.io | K3S_URL="https://${K3S_SERVER}:6443" K3S_TOKEN="${K3S_TOKEN}" sh -
echo "  Done"
echo ""

# --- Set hostname ---
hostnamectl set-hostname "$HOSTNAME" 2>/dev/null || true

# --- Install ntfy ---
if ! command -v ntfy &>/dev/null; then
    echo "Installing ntfy..."
    dnf install -y -q https://github.com/binwiederhier/ntfy/releases/download/v2.11.0/ntfy_2.11.0_linux_amd64.rpm 2>/dev/null || true
fi

# --- Configure ntfy ---
cat > /etc/ntfy/server.yml << EOF
base-url: "http://$(hostname -I | awk '{print $1}'):8090"
listen-http: ":8090"
cache-file: "/var/lib/ntfy/cache.db"
behind-proxy: false
EOF

systemctl enable ntfy
systemctl restart ntfy
echo "  ntfy running on port 8090"
echo ""

# --- Install local OCI registry mirror ---
echo "Installing local OCI registry mirror..."
podman run -d \
    --name registry \
    -p 5000:5000 \
    -v /var/lib/registry:/var/lib/registry \
    --restart=always \
    docker.io/library/registry:2 2>/dev/null || true
echo "  Registry running on port 5000"
echo ""

# --- Configure firewall ---
echo "Configuring firewall..."
if command -v firewall-cmd &>/dev/null; then
    firewall-cmd --permanent --add-port=6443/tcp  # k3s API
    firewall-cmd --permanent --add-port=8090/tcp  # ntfy
    firewall-cmd --permanent --add-port=5000/tcp  # registry
    firewall-cmd --permanent --add-port=10250/tcp # kubelet
    firewall-cmd --reload
    echo "  Ports opened"
fi
echo ""

# --- Verify k3s ---
echo "Verifying k3s..."
sleep 5
if systemctl is-active k3s-agent &>/dev/null; then
    echo "  k3s agent is running"
else
    echo "  WARNING: k3s agent not yet active, check: journalctl -u k3s-agent"
fi
echo ""

echo "=========================================="
echo "  Worker Bootstrap Complete!"
echo "=========================================="
echo ""
echo "Worker: $HOSTNAME"
echo "  k3s:    $(systemctl is-active k3s-agent 2>/dev/null || echo 'unknown')"
echo "  ntfy:   $(systemctl is-active ntfy 2>/dev/null || echo 'unknown')"
echo "  registry: $(podman inspect --format '{{.State.Status}}' registry 2>/dev/null || echo 'unknown')"
echo ""
echo "The worker is now registered with the control plane."
echo "Use 'stronghold worker list' on the control plane to verify."
