# Multi-Box Fleet Deployment Example

Control plane on one box, multiple k3s workers. Suitable for 5+ concurrent agents.

## Requirements

- 1 SEV-SNP-capable Vultr box for the control plane
- 1+ Vultr boxes for workers (SEV-SNP optional)
- Tailscale for mesh networking

## Steps

```bash
# 1. Provision and bootstrap the control plane (SEV-SNP)
ssh root@control-plane
curl -sL https://github.com/pkhairkh/stronghold/releases/latest/download/bootstrap.sh | bash

# 2. Install Tailscale on all boxes
curl -fsSL https://tailscale.com/install.sh | sh
tailscale up

# 3. Provision and bootstrap workers
ssh root@worker-1
bash worker-bootstrap.sh \
  --host vultr-worker-1.fra1 \
  --token <k3s-token> \
  --server <control-plane-tailscale-ip>

ssh root@worker-2
bash worker-bootstrap.sh \
  --host vultr-worker-2.fra1 \
  --token <k3s-token> \
  --server <control-plane-tailscale-ip>

# 4. On the control plane, verify workers
stronghold worker list
# vultr-worker-1.fra1   8 cpu / 16GB   sev-snp: yes   0 pods
# vultr-worker-2.fra1   8 cpu / 16GB   sev-snp: no    0 pods

# 5. Create tenants
stronghold tenant create --name "alice"
stronghold tenant create --name "bob"

# 6. Each tenant enrolls their phone and mints agent tokens
```

## Architecture

```
┌──────────────────────┐
│  Control Plane       │
│  (SEV-SNP Vultr)     │
│  stronghold gateway  │
│  ntfy                │
│  k3s server          │
└──────────┬───────────┘
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
 containerd pods (per-agent workspaces)
```

## VPS Escalation

For GPU or large-memory workloads:

```bash
curl -X POST https://gateway:8443/agent/order \
  -d '{
    "image": "stronghold/python-ml:2026.07",
    "compute": { "dedicated": true, "gpu": true, "memory_gb": 64 }
  }'
```

The gateway boots a dedicated Vultr VPS with GPU, schedules the pod, and destroys the VPS when the session ends.
