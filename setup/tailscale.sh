#!/usr/bin/env bash
# Stronghold — optional Tailscale installation and configuration
#
# Per W10-T6 DoD: install Tailscale, configure to only expose gateway ports
# (8443, 8090) on the tailnet's Tailscale IP, leaving other internal ports
# (6443, 10250, 5000) reachable only from inside the tailnet.
#
# Idempotent: safe to re-run.
#
# Usage:
#   bash setup/tailscale.sh                          # install + interactive up
#   bash setup/tailscale.sh --auth-key=tskey-...     # unattended join
#   bash setup/tailscale.sh --hostname=stronghold-cp
#   bash setup/tailscale.sh --advertise-routes=10.0.0.0/24
#   bash setup/tailscale.sh --status                 # show status only

set -euo pipefail

AUTH_KEY=""
TS_HOSTNAME=""
ADVERTISE_ROUTES=""
ACCEPT_ROUTES=false
STATUS_ONLY=false
EXIT_NODE=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --auth-key=*)         AUTH_KEY="${1#*=}" ;;
        --auth-key)           AUTH_KEY="$2"; shift ;;
        --hostname=*)         TS_HOSTNAME="${1#*=}" ;;
        --hostname)           TS_HOSTNAME="$2"; shift ;;
        --advertise-routes=*) ADVERTISE_ROUTES="${1#*=}" ;;
        --advertise-routes)   ADVERTISE_ROUTES="$2"; shift ;;
        --accept-routes)      ACCEPT_ROUTES=true ;;
        --exit-node)          EXIT_NODE=true ;;
        --status)             STATUS_ONLY=true ;;
        --help|-h)
            cat <<EOF
Usage: tailscale.sh [OPTIONS]

Options:
  --auth-key=KEY          Tailscale auth key (for unattended join)
  --hostname=NAME         Tailscale hostname (default: system hostname)
  --advertise-routes=CIDR Comma-separated CIDRs to advertise (e.g. 10.0.0.0/24)
  --accept-routes         Accept advertised routes from other nodes
  --exit-node             Configure this box as a Tailscale exit node
  --status                Print status only and exit
  -h, --help              Show this help

Environment:
  TAILSCALE_AUTH_KEY      Alternative to --auth-key

Notes:
  - Requires a Tailscale account. Sign up at https://login.tailscale.com
  - Auth keys are created at https://login.tailscale.com/admin/settings/keys
  - This script enables IP forwarding if --advertise-routes is set
EOF
            exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
    shift
done

# Use env var fallback
if [[ -z "$AUTH_KEY" && -n "${TAILSCALE_AUTH_KEY:-}" ]]; then
    AUTH_KEY="$TAILSCALE_AUTH_KEY"
fi

if [[ "$EUID" -ne 0 ]]; then
    echo "ERROR: Run as root" >&2
    exit 2
fi

# --- Status-only mode ---
if [[ "$STATUS_ONLY" == "true" ]]; then
    if ! command -v tailscale &>/dev/null; then
        echo "Tailscale not installed"
        exit 1
    fi
    tailscale status
    echo ""
    echo "IP addresses:"
    tailscale ip -4 2>/dev/null || true
    tailscale ip -6 2>/dev/null || true
    exit 0
fi

echo "=========================================="
echo "  Tailscale Installation & Configuration"
echo "=========================================="
echo ""

# --- Install (idempotent) ---
if ! command -v tailscale &>/dev/null; then
    echo "[*] Installing Tailscale..."
    curl -fsSL https://tailscale.com/install.sh | sh
    echo "[+] Tailscale installed"
else
    echo "[+] Tailscale already installed: $(tailscale version | head -1)"
fi

systemctl enable --now tailscaled
echo ""

# --- Enable IP forwarding if advertising routes ---
if [[ -n "$ADVERTISE_ROUTES" ]]; then
    echo "[*] Enabling IP forwarding for route advertisement..."
    cat > /etc/sysctl.d/99-stronghold-tailscale.conf <<EOF
net.ipv4.ip_forward = 1
net.ipv6.conf.all.forwarding = 1
EOF
    sysctl --system 2>/dev/null | tail -1 || true
    echo "[+] IP forwarding enabled"
    echo ""
fi

# --- Build tailscale up args ---
TS_ARGS=(--accept-dns=false)
if [[ -n "$TS_HOSTNAME" ]]; then
    TS_ARGS+=(--hostname="$TS_HOSTNAME")
fi
if [[ -n "$ADVERTISE_ROUTES" ]]; then
    TS_ARGS+=(--advertise-routes="$ADVERTISE_ROUTES")
fi
if [[ "$ACCEPT_ROUTES" == "true" ]]; then
    TS_ARGS+=(--accept-routes)
fi
if [[ "$EXIT_NODE" == "true" ]]; then
    TS_ARGS+=(--advertise-exit-node)
fi

# --- Authenticate ---
if tailscale status &>/dev/null; then
    echo "[+] Already authenticated to tailnet"
    if [[ ${#TS_ARGS[@]} -gt 0 ]]; then
        echo "[*] Re-applying tailscale up with new options..."
        tailscale up "${TS_ARGS[@]}" || true
    fi
else
    if [[ -z "$AUTH_KEY" ]]; then
        echo "[*] No auth key provided. Running interactive 'tailscale up'..."
        echo "    Open the URL that appears in a browser to authenticate."
        echo ""
        # We can't pass --advertise-routes without auth-key in non-interactive mode
        # but `tailscale up` will print a URL the user must visit.
        tailscale up "${TS_ARGS[@]}" || true
    else
        echo "[*] Authenticating with auth key (unattended)..."
        if [[ ${#TS_ARGS[@]} -gt 0 ]]; then
            tailscale up --auth-key="$AUTH_KEY" "${TS_ARGS[@]}"
        else
            tailscale up --auth-key="$AUTH_KEY"
        fi
        echo "[+] Authenticated to tailnet"
    fi
fi
echo ""

# --- Restrict exposed ports on Tailscale interface ---
# Stronghold exposes 8443 (gateway) and 8090 (ntfy) publicly already.
# On the Tailscale interface, we also want 6443, 10250, 5000 reachable.
# These are configured by setup/firewall.sh which auto-detects the Tailscale
# interface. We invoke it here if not already configured.
TS_IFACE="$(ip -o link show 2>/dev/null \
    | awk -F': ' '{print $2}' \
    | grep -E '^tailscale[0-9]*$' \
    | head -1 || true)"

if [[ -n "$TS_IFACE" ]]; then
    echo "[+] Tailscale interface: $TS_IFACE"
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    if [[ -x "${SCRIPT_DIR}/firewall.sh" ]]; then
        echo "[*] Ensuring firewall allows internal ports on $TS_IFACE..."
        bash "${SCRIPT_DIR}/firewall.sh" --tailscale-iface="$TS_IFACE" || true
    fi
else
    echo "[!] No Tailscale interface detected yet — run firewall.sh after 'tailscale up' completes."
fi
echo ""

# --- Print summary ---
echo "=========================================="
echo "  Tailscale Configured"
echo "=========================================="
echo ""
echo "  Hostname:   $(tailscale status --json 2>/dev/null | jq -r '.Self.HostName // "unknown"')"
echo "  Tailscale IP (v4): $(tailscale ip -4 2>/dev/null || echo 'none')"
echo "  Tailscale IP (v6): $(tailscale ip -6 2>/dev/null || echo 'none')"
echo ""
echo "  Internal services reachable from the tailnet:"
echo "    https://$(tailscale ip -4 2>/dev/null || echo '<ts-ip>'):8443   (gateway)"
echo "    http://$(tailscale ip -4 2>/dev/null || echo '<ts-ip>'):8090    (ntfy)"
echo "    https://$(tailscale ip -4 2>/dev/null || echo '<ts-ip>'):6443   (k3s API)"
echo ""
echo "  Other boxes on the tailnet can now reach this box for k3s/registry."
echo ""
echo "Useful commands:"
echo "  tailscale status           # show tailnet"
echo "  tailscale ping <hostname>  # verify connectivity"
echo "  tailscale logout           # leave tailnet"
