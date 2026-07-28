#!/usr/bin/env bash
# Stronghold firewall configuration
#
# Per W10-T5 DoD:
#   - 8443/tcp (gateway)  open publicly
#   - 8090/tcp (ntfy)     open publicly
#   - 6443/tcp (k3s API)  Tailscale interface ONLY
#   - 10250/tcp (kubelet) Tailscale interface ONLY
#   - 5000/tcp (registry) Tailscale interface ONLY
#   - 8472/udp (k3s flannel VXLAN)  Tailscale interface ONLY
#
# Idempotent: safe to re-run. Uses firewalld on Rocky Linux 9/10.
#
# Usage:
#   bash setup/firewall.sh                 # auto-detect Tailscale interface
#   bash setup/firewall.sh --tailscale-iface tailscale0
#   bash setup/firewall.sh --public-only   # only 8443 + 8090; defer internal
#   bash setup/firewall.sh --reset         # remove all Stronghold rules

set -euo pipefail

TS_IFACE=""
PUBLIC_ONLY=false
RESET=false
ROLE="${STRONGHOLD_ROLE:-control-plane}"  # or "worker"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tailscale-iface=*) TS_IFACE="${1#*=}" ;;
        --tailscale-iface)   TS_IFACE="$2"; shift ;;
        --public-only)       PUBLIC_ONLY=true ;;
        --reset)             RESET=true ;;
        --role=*)            ROLE="${1#*=}" ;;
        --help|-h)
            cat <<EOF
Usage: firewall.sh [OPTIONS]

Options:
  --tailscale-iface=IFACE   Tailscale interface name (default: auto-detect)
  --public-only             Only open 8443 + 8090; defer internal ports
  --reset                   Remove all Stronghold firewall rules
  --role=ROLE               "control-plane" (default) or "worker"
  -h, --help                Show this help
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

if ! command -v firewall-cmd &>/dev/null; then
    echo "ERROR: firewalld not installed. Install with: dnf install -y firewalld" >&2
    exit 1
fi
systemctl enable --now firewalld 2>/dev/null || true

# Auto-detect Tailscale interface if not specified
if [[ -z "$TS_IFACE" && "$PUBLIC_ONLY" == "false" ]]; then
    TS_IFACE="$(ip -o link show 2>/dev/null \
        | awk -F': ' '{print $2}' \
        | grep -E '^tailscale[0-9]*$' \
        | head -1 || true)"
    if [[ -z "$TS_IFACE" ]]; then
        echo "WARNING: No Tailscale interface detected."
        echo "         Internal ports (6443, 10250, 5000) will not be opened."
        echo "         Install Tailscale first (setup/tailscale.sh) or specify --tailscale-iface."
        PUBLIC_ONLY=true
    fi
fi

# --- Helper: idempotent add/remove ---
fw_add_port() {
    local port="$1" zone="${2:-public}"
    firewall-cmd --permanent --zone="$zone" --add-port="$port" 2>/dev/null || true
}
fw_del_port() {
    local port="$1" zone="${2:-public}"
    firewall-cmd --permanent --zone="$zone" --remove-port="$port" 2>/dev/null || true
}

if [[ "$RESET" == "true" ]]; then
    echo "[*] Resetting Stronghold firewall rules..."
    for p in 8443/tcp 8090/tcp 6443/tcp 10250/tcp 5000/tcp 8472/udp; do
        fw_del_port "$p" public
        fw_del_port "$p" trusted
    done
    firewall-cmd --reload
    echo "[+] Rules removed"
    exit 0
fi

echo "=========================================="
echo "  Stronghold Firewall Configuration"
echo "=========================================="
echo "  Role:                $ROLE"
echo "  Tailscale interface: ${TS_IFACE:-none}"
echo "  Public-only mode:    $PUBLIC_ONLY"
echo ""

# --- Public ports (always open) ---
echo "[*] Opening public ports: 8443/tcp (gateway), 8090/tcp (ntfy)..."
fw_add_port 8443/tcp
fw_add_port 8090/tcp
echo "[+] Public ports opened on default zone"
echo ""

if [[ "$PUBLIC_ONLY" == "true" ]]; then
    echo "[*] --public-only: skipping internal ports"
    firewall-cmd --reload
    exit 0
fi

# --- Tailscale zone setup ---
# Create a "trusted" zone bound to the Tailscale interface for internal traffic.
# Existing trusted zone is fine; we just add the interface and ports.
if ! firewall-cmd --get-zones 2>/dev/null | tr ' ' '\n' | grep -qx trusted; then
    firewall-cmd --permanent --new-zone=trusted 2>/dev/null || true
    firewall-cmd --permanent --zone=trusted --set-target=ACCEPT 2>/dev/null || true
fi
if [[ -n "$TS_IFACE" ]]; then
    firewall-cmd --permanent --zone=trusted --add-interface="$TS_IFACE" 2>/dev/null || true
    # Allow traffic on this interface — bind internal ports to this zone only
    echo "[*] Binding $TS_IFACE to trusted zone..."
fi

# Internal ports — bound to Tailscale (trusted zone)
echo "[*] Opening internal ports (Tailscale-only): 6443/tcp, 10250/tcp, 5000/tcp, 8472/udp..."
INTERNAL_PORTS=(6443/tcp 10250/tcp 5000/tcp 8472/udp)
for p in "${INTERNAL_PORTS[@]}"; do
    fw_add_port "$p" trusted
    # Belt-and-braces: also remove from public zone (in case a previous run added them)
    fw_del_port "$p" public
done
echo "[+] Internal ports opened on trusted zone (Tailscale only)"
echo ""

# --- k3s flannel VXLAN needs to be allowed between nodes ---
# If using Tailscale, flannel runs over the TS interface, so the rule above
# suffices. If not, we open 8472/udp on public — but that's insecure.
# We only do this on worker nodes for the worker→worker path.
if [[ "$ROLE" == "worker" && -z "$TS_IFACE" ]]; then
    echo "[!] No Tailscale interface; opening 8472/udp (flannel) publicly."
    echo "    This is INSECURE — install Tailscale (setup/tailscale.sh)."
    fw_add_port 8472/udp
fi

# --- Reload ---
firewall-cmd --reload
echo ""
echo "[+] Firewall configured. Current state:"
echo ""
echo "  Default (public) zone:"
firewall-cmd --zone=public --list-ports 2>/dev/null | sed 's/^/    /'
if [[ -n "$TS_IFACE" ]]; then
    echo "  Trusted (Tailscale) zone:"
    firewall-cmd --zone=trusted --list-ports 2>/dev/null | sed 's/^/    /'
fi
echo ""
echo "Verify from an external host:"
echo "  nmap -p 8443,8090,6443,10250,5000 <this-host>"
echo "  Expected: 8443, 8090 open; 6443, 10250, 5000 filtered"
