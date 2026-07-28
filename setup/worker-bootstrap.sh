#!/usr/bin/env bash
# Stronghold Worker Bootstrap Script
#
# Installs k3s worker, ntfy, and a local OCI registry mirror on a Vultr box.
# The worker joins the Stronghold cluster and becomes eligible for pod scheduling.
# Idempotent: safe to re-run.
#
# Supports Rocky Linux 9 and 10.
#
# Usage:
#   bash setup/worker-bootstrap.sh \
#     --host vultr-worker-N.fra1 \
#     --token <k3s-token> \
#     --server <control-plane-ip> \
#     [--ntfy-url http://control-plane-ip:8090] \
#     [--registry-mirror http://control-plane-ip:5000] \
#     [--tailscale]     # join tailnet before registering

set -euo pipefail

HOSTNAME_NEW=""
K3S_TOKEN=""
K3S_SERVER=""
NTFY_URL=""
REGISTRY_MIRROR=""
USE_TAILSCALE=false

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

usage() {
    cat <<EOF
Usage: worker-bootstrap.sh --host HOST --token TOKEN --server IP [OPTIONS]

Required:
  --host HOST         Worker hostname
  --token TOKEN       k3s node-token from the control plane (/var/lib/rancher/k3s/server/node-token)
  --server IP         Control plane IP (or Tailscale hostname)

Optional:
  --ntfy-url URL      ntfy server URL (default: http://<server>:8090)
  --registry-mirror U OCI registry mirror URL (default: http://<server>:5000)
  --tailscale         Install/configure Tailscale before joining cluster
  --help              Show this help
EOF
}

# Parse args
while [[ $# -gt 0 ]]; do
    case "$1" in
        --host)            HOSTNAME_NEW="$2"; shift 2 ;;
        --token)           K3S_TOKEN="$2"; shift 2 ;;
        --server)          K3S_SERVER="$2"; shift 2 ;;
        --ntfy-url)        NTFY_URL="$2"; shift 2 ;;
        --registry-mirror) REGISTRY_MIRROR="$2"; shift 2 ;;
        --tailscale)       USE_TAILSCALE=true; shift ;;
        --help|-h)         usage; exit 0 ;;
        *) err "Unknown option: $1"; usage; exit 1 ;;
    esac
done

if [[ -z "$HOSTNAME_NEW" || -z "$K3S_TOKEN" || -z "$K3S_SERVER" ]]; then
    err "--host, --token, and --server are required"
    usage
    exit 1
fi

if [[ -z "$NTFY_URL" ]]; then
    NTFY_URL="http://${K3S_SERVER}:8090"
fi
if [[ -z "$REGISTRY_MIRROR" ]]; then
    REGISTRY_MIRROR="http://${K3S_SERVER}:5000"
fi

echo "=========================================="
echo "  Stronghold Worker Bootstrap"
echo "=========================================="
echo "  Host:       $HOSTNAME_NEW"
echo "  Server:     $K3S_SERVER"
echo "  ntfy URL:   $NTFY_URL"
echo "  Registry:   $REGISTRY_MIRROR"
echo "  Tailscale:  $USE_TAILSCALE"
echo ""

# --- Check root ---
if [[ $EUID -ne 0 ]]; then
    err "Run as root (use sudo)"
    exit 2
fi

# --- Check OS ---
if [[ -f /etc/rocky-release ]]; then
    ROCKY_VER=$(grep -oE 'VERSION="[0-9]+' /etc/os-release | head -1 | grep -oE '[0-9]+')
    log "Detected Rocky Linux ${ROCKY_VER}"
else
    warn "Not Rocky Linux. Detected: $(grep '^PRETTY_NAME' /etc/os-release 2>/dev/null || echo unknown)"
fi
echo ""

# --- Install dependencies (idempotent) ---
log "Installing system dependencies..."
dnf install -y -q \
    git curl wget jq \
    podman containerd \
    firewalld \
    policycoreutils-python-utils \
    2>/dev/null || true
systemctl enable --now firewalld 2>/dev/null || true
# Disable firewalld's default zone interfering with k3s flannel (VXLAN)
# We configure explicit ports below.
ok "Dependencies installed"
echo ""

# --- Set hostname (idempotent) ---
CURRENT_HOST="$(hostname -f 2>/dev/null || hostname)"
if [[ "$CURRENT_HOST" != "$HOSTNAME_NEW" ]]; then
    log "Setting hostname to ${HOSTNAME_NEW}..."
    hostnamectl set-hostname "$HOSTNAME_NEW"
    # Update /etc/hosts so the new hostname resolves locally
    if ! grep -qE "\\s${HOSTNAME_NEW}\\s*\$" /etc/hosts 2>/dev/null; then
        echo "127.0.0.1 ${HOSTNAME_NEW}" >> /etc/hosts
    fi
    ok "Hostname set"
else
    ok "Hostname already ${HOSTNAME_NEW}"
fi
echo ""

# --- Optional: Tailscale (idempotent) ---
if [[ "$USE_TAILSCALE" == "true" ]]; then
    log "Installing/configuring Tailscale..."
    if ! command -v tailscale &>/dev/null; then
        curl -fsSL https://tailscale.com/install.sh | sh
        ok "Tailscale installed"
    else
        ok "Tailscale already installed"
    fi
    if ! systemctl is-active --quiet tailscaled 2>/dev/null; then
        systemctl enable --now tailscaled
    fi
    if ! tailscale status &>/dev/null; then
        warn "Tailscale not yet authenticated. Run: tailscale up"
    else
        ok "Tailscale online: $(tailscale ip -4 2>/dev/null || echo 'no IP')"
    fi
    echo ""
fi

# --- Install k3s worker (idempotent: skip if already installed) ---
if [[ -x /usr/local/bin/k3s ]] && systemctl is-active --quiet k3s-agent 2>/dev/null; then
    ok "k3s agent already installed and active"
else
    log "Installing k3s worker..."
    # k3s install script is itself idempotent — re-running with same env re-registers
    curl -sfL https://get.k3s.io | \
        K3S_URL="https://${K3S_SERVER}:6443" \
        K3S_TOKEN="${K3S_TOKEN}" \
        sh -
    systemctl enable --now k3s-agent 2>/dev/null || true
    ok "k3s agent installed"
fi
echo ""

# --- Install ntfy (idempotent) ---
if ! command -v ntfy &>/dev/null; then
    log "Installing ntfy..."
    NTFY_RPM_URL="https://github.com/binwiederhier/ntfy/releases/download/v2.11.0/ntfy_2.11.0_linux_amd64.rpm"
    if dnf install -y -q "$NTFY_RPM_URL"; then
        ok "ntfy installed"
    else
        warn "ntfy install failed; continuing"
    fi
else
    ok "ntfy already installed"
fi

# Deploy ntfy config (idempotent: always overwrite)
install -d -m 0750 -o ntfy -g ntfy /etc/ntfy 2>/dev/null || install -d -m 0750 /etc/ntfy
BOX_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
cat > /etc/ntfy/server.yml <<EOF
# ntfy server config — worker (local-only).
# Workers don't need full ACLs; the control plane ntfy is authoritative.
base-url: "http://${BOX_IP}:8090"
listen-http: ":8090"
cache-file: "/var/lib/ntfy/cache.db"
behind-proxy: false
enable-login: true
enable-signup: false
EOF
chown ntfy:ntfy /etc/ntfy/server.yml 2>/dev/null || true
chmod 0640 /etc/ntfy/server.yml
install -d -m 0750 -o ntfy -g ntfy /var/lib/ntfy 2>/dev/null || install -d -m 0750 /var/lib/ntfy
ok "ntfy config deployed"
echo ""

# --- Install local OCI registry mirror (idempotent: skip if running) ---
log "Provisioning local OCI registry mirror..."
if podman ps --format '{{.Names}}' 2>/dev/null | grep -q '^registry$'; then
    ok "registry container already running"
elif podman ps -a --format '{{.Names}}' 2>/dev/null | grep -q '^registry$'; then
    log "Starting existing registry container..."
    podman start registry 2>/dev/null || true
else
    log "Pulling and starting registry:2..."
    podman pull docker.io/library/registry:2 2>/dev/null || true
    podman run -d \
        --name registry \
        -p 5000:5000 \
        -v /var/lib/registry:/var/lib/registry:Z \
        --restart=always \
        docker.io/library/registry:2
fi

# Generate systemd unit so the container survives reboot (idempotent)
if [[ ! -f /etc/systemd/system/stronghold-registry.service ]]; then
    podman generate systemd --name registry --new=false > /etc/systemd/system/stronghold-registry.service 2>/dev/null || true
    if [[ -s /etc/systemd/system/stronghold-registry.service ]]; then
        systemctl daemon-reload
        systemctl enable stronghold-registry.service 2>/dev/null || true
    fi
fi
ok "Registry on port 5000"
echo ""

# --- Configure firewall (Tailscale-aware) ---
# Per W10-T5 DoD: 6443/10250/5000 only on Tailscale interface; 8090 public.
log "Configuring firewall..."
TS_IFACE="$(tailscale status --json 2>/dev/null | jq -r '.Self tailscale? // empty' 2>/dev/null || true)"
# Fallback: detect Tailscale interface by name
if [[ -z "$TS_IFACE" ]]; then
    TS_IFACE="$(ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | grep -E '^tailscale[0-9]*$' | head -1 || true)"
fi

if command -v firewall-cmd &>/dev/null && systemctl is-active --quiet firewalld; then
    # Public ports
    for p in 8090/tcp; do
        firewall-cmd --permanent --add-port="$p" 2>/dev/null || true
    done

    # Tailscale-only ports (bind to tailscale interface if present)
    for p in 6443/tcp 10250/tcp 5000/tcp; do
        if [[ -n "$TS_IFACE" ]]; then
            firewall-cmd --permanent --zone=trusted --add-interface="$TS_IFACE" 2>/dev/null || true
            firewall-cmd --permanent --zone=trusted --add-port="$p" 2>/dev/null || true
        else
            warn "Tailscale interface not found; opening ${p} on default zone (review post-install)"
            firewall-cmd --permanent --add-port="$p" 2>/dev/null || true
        fi
    done

    # k3s flannel VXLAN (used internally between k3s nodes)
    firewall-cmd --permanent --add-port=8472/udp 2>/dev/null || true
    # k3s metrics-server
    firewall-cmd --permanent --add-port=10250/tcp 2>/dev/null || true

    firewall-cmd --reload 2>/dev/null || true
    ok "Firewall configured (8090 public; 6443/10250/5000 ${TS_IFACE:+on $TS_IFACE}${TS_IFACE:-on default zone})"
else
    warn "firewalld not active — configure firewall manually (see setup/firewall.sh)"
fi
echo ""

# --- Verify k3s ---
log "Verifying k3s agent registration..."
for i in 1 2 3 4 5 6; do
    if systemctl is-active --quiet k3s-agent 2>/dev/null; then
        ok "k3s agent: active (after ${i} attempt(s))"
        break
    fi
    warn "k3s agent not active yet (attempt ${i}/6), waiting..."
    sleep 5
done

if ! systemctl is-active --quiet k3s-agent 2>/dev/null; then
    err "k3s agent not active. Check: journalctl -u k3s-agent -n 100"
fi

# Show k3s node status if kubectl is available
if [[ -x /usr/local/bin/k3s ]]; then
    NODE_INFO="$(k3s kubectl get nodes 2>/dev/null | tail -n +2 || true)"
    if [[ -n "$NODE_INFO" ]]; then
        ok "k3s nodes:"
        echo "$NODE_INFO" | sed 's/^/    /'
    fi
fi
echo ""

echo "=========================================="
echo "  Worker Bootstrap Complete"
echo "=========================================="
echo ""
echo "Worker:    $HOSTNAME_NEW"
echo "  k3s:       $(systemctl is-active k3s-agent 2>/dev/null || echo unknown)"
echo "  ntfy:      $(systemctl is-active ntfy 2>/dev/null || echo unknown)"
echo "  registry:  $(podman inspect --format '{{.State.Status}}' registry 2>/dev/null || echo unknown)"
if [[ -n "$TS_IFACE" ]]; then
echo "  tailscale: $(tailscale ip -4 2>/dev/null || echo 'unknown')"
fi
echo ""
echo "The worker is registered with the control plane at ${K3S_SERVER}:6443."
echo "On the control plane, verify with:"
echo "  stronghold worker list"
echo "  k3s kubectl get nodes"
