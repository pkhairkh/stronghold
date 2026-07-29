# Community-Hosted Deployment Example

Stronghold as a service for multiple tenants. Each tenant has their own credentials, quotas, and audit logs.

## Requirements

- Production-grade control plane (SEV-SNP, 16+ vCPU, 32+ GB RAM)
- 3+ worker boxes
- External etcd for HA control plane (3 nodes)
- S3-compatible object storage for backups
- Tailscale for mesh networking

## Steps

```bash
# 1. Set up HA etcd cluster (3 boxes)
# On each etcd box:
dnf install -y etcd
# Configure etcd cluster...

# 2. Provision the control plane (SEV-SNP)
ssh root@control-plane
curl -sL https://github.com/pkhairkh/stronghold/releases/latest/download/bootstrap.sh | bash

# Configure HA mode
stronghold ha init --etcd-endpoints etcd1,etcd2,etcd3

# 3. Add workers
stronghold worker add --host worker-1 --token <token>
stronghold worker add --host worker-2 --token <token>
stronghold worker add --host worker-3 --token <token>

# 4. Create tenants
stronghold tenant create --name "alice" --max-concurrent-machines 3
stronghold tenant create --name "bob" --max-concurrent-machines 5
stronghold tenant create --name "charlie" --max-concurrent-machines 2

# 5. Each tenant:
#    - Opens the enrollment URL on their phone
#    - Verifies the SEV-SNP measurement
#    - Enrolls their own WebAuthn credential(s)
#    - Mints their own agent tokens
#    - Configures their own scopes, anomaly patterns, and network policies
```

## Multi-Tenant Isolation

| Layer | Isolation |
|---|---|
| Audit logs | Separate SQLite DB per tenant |
| Credentials | Per-tenant WebAuthn credentials |
| Agent tokens | Per-tenant, scoped, TTL'd |
| Quotas | Per-tenant CPU/memory/disk caps |
| Network policies | Per-tenant egress allowlists |
| Pods | Cannot communicate across tenants (network policy) |
| Processes | Cannot see each other (PID namespace) |
| Filesystems | Cannot see each other (mount namespace) |

## Billing (Optional)

Stronghold does not include billing. To charge tenants:

1. Track resource usage per tenant:
   ```bash
   stronghold audit export --tenant alice --format json | jq '[.[] | .payload.cpu_hours] | add'
   ```

2. Export usage data to your billing system (Stripe, etc.)

3. Set per-tenant quotas to enforce plan limits:
   ```bash
   stronghold tenant quota set --tenant alice \
     --max-concurrent-machines 3 \
     --total-cpu-budget 16 \
     --total-memory-gb-budget 64
   ```

## Backup Strategy

```bash
# Daily backup to S3
stronghold backup --to s3://stronghold-backups/$(date +%Y%m%d).tar.gz

# Retention: 30 days
# Configure S3 lifecycle policy to delete after 30 days
```

## Monitoring

```bash
# Gateway health
curl https://gateway:8443/agent/health

# Per-tenant stats
stronghold tenant get --id <tenant-id>

# Worker capacity
stronghold worker list

# Audit verification (automated daily)
stronghold audit verify --tenant alice
stronghold audit verify --tenant bob
stronghold audit verify --tenant charlie
```
