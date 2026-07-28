# Stronghold Deployment Guide

## Deployment Patterns

Stronghold supports three deployment patterns, from simplest to most complex:

1. **Single-box** — Control plane + one worker on the same Vultr box
2. **Multi-box fleet** — Control plane on one box, multiple k3s workers
3. **Community-hosted** — Stronghold as a service for multiple tenants

---

## Pattern 1: Single-Box (Development / Small Scale)

All components on one Vultr box. Suitable for 1-3 concurrent agents.

### Requirements

- Vultr High Frequency plan with AMD SEV-SNP
- 8+ vCPU, 16+ GB RAM, 200+ GB NVMe
- Rocky Linux 9

### Setup

```bash
# 1. Provision a Vultr box with SEV-SNP
#    Plan: HF-8C-32GB (or larger)
#    Region: Any that supports SEV-SNP
#    OS: Rocky Linux 9

# 2. SSH in and bootstrap
curl -sL https://github.com/pkhairkh/stronghold/releases/latest/download/bootstrap.sh | bash

# 3. Verify SEV-SNP
ls -la /dev/sev
# Should show: crw------- 1 root root 10, 124 ... /dev/sev

# 4. Enroll your phone
# Open the printed URL in your phone browser

# 5. Mint an agent token
stronghold agent-token mint --tenant default --ttl 86400
```

### Architecture

```
┌─────────────────────────────────────┐
│  Vultr Box (SEV-SNP)               │
│                                     │
│  ┌─────────────┐  ┌─────────────┐   │
│  │ stronghold  │  │ ntfy        │   │
│  │ gateway     │  │ (port 8090) │   │
│  │ (port 8443) │  └─────────────┘   │
│  └──────┬──────┘                     │
│         │                            │
│  ┌──────▼──────┐                     │
│  │ k3s worker  │                     │
│  │ (single)    │                     │
│  └──────┬──────┘                     │
│         │                            │
│  ┌──────▼──────┐                     │
│  │ containerd  │                     │
│  │ pods        │                     │
│  └─────────────┘                     │
└─────────────────────────────────────┘
```

### Limitations

- Max concurrent agents bounded by box RAM (each agent gets 4GB cgroup cap by default)
- No redundancy — if the box goes down, all sessions are lost
- Single point of failure for the control plane

---

## Pattern 2: Multi-Box Fleet (Production)

Control plane on one box, multiple k3s workers. Suitable for 5+ concurrent agents or when you need redundancy.

### Requirements

- 1 SEV-SNP-capable Vultr box for the control plane
- 1+ Vultr boxes for workers (SEV-SNP optional)
- Tailscale or WireGuard mesh between boxes

### Setup

```bash
# 1. Provision the control plane box (SEV-SNP)
curl -sL https://github.com/pkhairkh/stronghold/releases/latest/download/bootstrap.sh | bash

# 2. Provision worker boxes
# On each worker:
bash worker-bootstrap.sh \
  --host vultr-worker-N.region \
  --token <k3s-token> \
  --server <control-plane-ip>

# 3. On the control plane, verify workers
stronghold worker list
```

### Architecture

```
┌──────────────────────┐
│  Control Plane       │
│  (SEV-SNP Vultr)     │
│  ┌────────────────┐  │
│  │ stronghold     │  │
│  │ gateway        │  │
│  │ ntfy           │  │
│  └───────┬────────┘  │
└──────────┼───────────┘
           │ Tailscale mesh
    ┌──────┼──────┐
    ▼      ▼      ▼
┌──────┐┌──────┐┌──────┐
│Worker││Worker││Worker│
│ k3s  ││ k3s  ││ k3s  │
│ntfy  ││ntfy  ││ntfy  │
│reg.  ││reg.  ││reg.  │
└──┬───┘└──┬───┘└──┬───┘
   │       │       │
   ▼       ▼       ▼
 containerd pods
```

### VPS Escalation

For workloads needing more than any worker has (GPU, large memory):

```bash
# Agent requests dedicated VPS
curl -X POST https://gateway:8443/agent/order \
  -d '{
    "image": "stronghold/python-ml:2026.07",
    "compute": { "dedicated": true, "gpu": true, "memory_gb": 64 }
  }'
```

The gateway:
1. Calls Vultr API to boot a fresh Rocky VPS with GPU
2. Cloud-init installs k3s worker, joins cluster
3. Pod is scheduled on the new VPS
4. On session end, VPS is destroyed, volumes snapshotted

---

## Pattern 3: Community-Hosted

Stronghold as a service for multiple tenants. Each tenant has their own credentials, quotas, and audit logs.

### Requirements

- Production-grade control plane (SEV-SNP, 16+ vCPU)
- Multiple worker boxes
- External etcd for HA control plane
- Backup strategy (S3-compatible object storage)

### Setup

```bash
# 1. Set up HA control plane (3 boxes with etcd)
stronghold ha init --etcd-endpoints etcd1,etcd2,etcd3

# 2. Add workers
stronghold worker add --host worker-1 --token <token>
stronghold worker add --host worker-2 --token <token>

# 3. Create tenants
stronghold tenant create --name "alice"
stronghold tenant create --name "bob"
stronghold tenant create --name "charlie"

# 4. Each tenant enrolls their own phone
# 5. Each tenant mints their own agent tokens
```

### Multi-Tenant Isolation

- Each tenant has a separate SQLite audit database
- Each tenant has separate credentials, tokens, quotas
- Each tenant has separate network policies (egress allowlists)
- Pods cannot communicate across tenants (network policy)
- Pods cannot see each other's processes (PID namespace)
- Pods cannot see each other's filesystems (mount namespace)

### Billing (if you choose to monetize)

Stronghold does not include billing. If you want to charge tenants:
- Track resource usage per tenant (CPU-hours, GB-hours)
- Export usage data via `stronghold audit export`
- Integrate with Stripe or your billing system

---

## Network Configuration

### Tailscale (Recommended)

```bash
# On each box:
curl -fsSL https://tailscale.com/install.sh | sh
tailscale up

# Verify
tailscale status
```

Tailscale provides:
- Encrypted mesh networking between boxes
- No public ports needed (except 8443 for phone access)
- ACLs to restrict which boxes can talk to which

### WireGuard (Alternative)

```bash
# Install WireGuard
dnf install -y wireguard-tools

# Generate keys on each box
wg genkey | tee privatekey | wg pubkey > publickey

# Configure peers
# /etc/wireguard/wg0.conf
```

### Firewall Rules

```bash
# Control plane
firewall-cmd --permanent --add-port=8443/tcp  # gateway (public)
firewall-cmd --permanent --add-port=8090/tcp  # ntfy (public or Tailscale-only)

# Workers
firewall-cmd --permanent --add-port=6443/tcp  # k3s API (Tailscale-only)
firewall-cmd --permanent --add-port=10250/tcp # kubelet (Tailscale-only)
firewall-cmd --permanent --add-port=8090/tcp  # ntfy (Tailscale-only)
firewall-cmd --permanent --add-port=5000/tcp  # registry (Tailscale-only)
```

---

## Monitoring

### Health Checks

```bash
# Gateway health
curl https://gateway:8443/agent/health

# ntfy health
curl http://gateway:8090/v1/health

# k3s health
kubectl get nodes

# Worker health
stronghold worker health-check --host vultr-worker-1
```

### Logs

```bash
# Gateway logs
journalctl -u stronghold-gateway -f

# ntfy logs
journalctl -u ntfy -f

# k3s logs
journalctl -u k3s -f
```

### Metrics

TODO: Prometheus metrics endpoint at `/metrics`

---

## Security Hardening

### SSH

```bash
# /etc/ssh/sshd_config
PasswordAuthentication no
PubkeyAuthentication yes
PermitRootLogin no
Port 2222  # non-default port

systemctl restart sshd
```

### Fail2ban

```bash
dnf install -y fail2ban
systemctl enable fail2ban
systemctl start fail2ban
```

### Automatic Updates

```bash
dnf install -y dnf-automatic
systemctl enable dnf-automatic.timer
systemctl start dnf-automatic.timer
```
