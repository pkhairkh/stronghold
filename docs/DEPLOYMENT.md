# Stronghold Deployment Guide

> ⚠️ **Alpha release — DO NOT DEPLOY IN PRODUCTION.**
>
> This runbook covers three deployment patterns, from simplest to most
> complex. Each pattern has step-by-step instructions, troubleshooting, and
> rollback procedures. All scripts are idempotent — safe to re-run.
>
> **Known gaps affecting this runbook:**
>
> - The gateway serves **plain HTTP on port 8443** (TLS is not wired into
>   server startup — see gap #1). Use `http://`, not `https://`, against the
>   gateway. Compensate with a transport-level VPN (Tailscale/WireGuard) in
>   any deployment that crosses an untrusted network.
> - **Prometheus metrics are NOT exposed** (gap #13). There is no `/metrics`
>   route.
> - **Per-tenant Kubernetes namespaces and NetworkPolicy objects are NOT
>   created** (gaps #14, #15). All pods land in `default`; `tenant_id` is
>   only a label. See [Roadmap](#roadmap).
> - **VPS escalation is a stub** (gap #9). Dedicated/GPU requests return
>   `"stub-vps-id"` / `"0.0.0.0"`.
> - **`worker add` and `worker list` are stubs** (gap #10).
>
> See [README → Known Limitations](../README.md#known-limitations) for the
> full list of alpha gaps.

## Deployment Patterns

| Pattern | When to use | SEV-SNP required | Boxes |
|---------|-------------|------------------|-------|
| **Single-box** | Dev, small fleets (1-3 agents) | Yes (or `--dev`) | 1 |
| **Multi-box fleet** | Production (5+ agents, redundancy) | Yes on control plane | 2+ |
| **Community-hosted** | Stronghold-as-a-service | Yes on control plane | 3+ (HA) |

Common requirements (all patterns):

- **OS:** Rocky Linux 9 or 10 (other RHEL-family works, untested)
- **Root:** All bootstrap scripts require root
- **Network:** Outbound HTTPS for package install + GitHub releases
- **Tailscale:** Strongly recommended for multi-box; near-mandatory for prod

---

## Pattern 1: Single-Box

All components on one Vultr box. Suitable for 1-3 concurrent agents.

### Requirements

- Vultr High Frequency plan with AMD SEV-SNP
- 8+ vCPU, 16+ GB RAM, 200+ GB NVMe
- Rocky Linux 9 or 10

### Step-by-step

```bash
# 1. Provision a Vultr box with SEV-SNP
#    Plan: HF-8C-32GB (or larger)
#    Region: any that supports SEV-SNP (e.g. EWR, FRA)
#    OS: Rocky Linux 9 or 10

# 2. SSH in as root
ssh root@<box-ip>

# 3. Clone the repo (or curl the bootstrap script from a release)
git clone https://github.com/pkhairkh/stronghold.git
cd stronghold

# 4. Bootstrap (production: requires /dev/sev)
bash setup/bootstrap.sh

#    For development on a box without SEV-SNP:
bash setup/bootstrap.sh --dev

# 5. Verify SEV-SNP
ls -la /dev/sev
# Should show: crw------- 1 root root 10, 124 ... /dev/sev

# 6. Verify services are running
systemctl status stronghold-gateway
systemctl status ntfy

# 7. Verify ports are open
ss -tlnp | grep -E '8443|8090'

# 8. Smoke-test the gateway
#    NOTE: TLS is not enabled (gap #1). Use http://, not https://.
curl http://localhost:8443/agent/health
curl    http://localhost:8090/v1/health

# 9. Save the setup password printed by bootstrap.sh

# 10. Configure firewall (defensive — bootstrap.sh already opens the basics)
bash setup/firewall.sh

# 11. (Optional) Install Tailscale for secure remote management
bash setup/tailscale.sh

# 12. (Optional) Install monitoring
bash setup/monitoring.sh --all

# 13. Enroll your phone
#     Open the printed URL in your phone browser
#     Enter the setup password
#     Verify the SEV-SNP measurement
#     Complete Face ID enrollment

# 14. Mint an agent token
stronghold agent-token mint --tenant default --ttl 86400
```

### Architecture

```
┌─────────────────────────────────────┐
│  Vultr Box (SEV-SNP)                │
│                                     │
│  ┌─────────────┐  ┌─────────────┐   │
│  │ stronghold  │  │ ntfy        │   │
│  │ gateway     │  │ (port 8090) │   │
│  │ (port 8443) │  └─────────────┘   │
│  └──────┬──────┘                     │
│         │                            │
│  ┌──────▼──────┐                     │
│  │ SQLite DB   │                     │
│  │ Audit log   │                     │
│  └─────────────┘                     │
│                                     │
│  ┌─────────────┐  ┌─────────────┐   │
│  │ node_expor- │  │ Prometheus  │   │
│  │ ter (:9100) │  │ (:9090)     │   │
│  └─────────────┘  └─────────────┘   │
└─────────────────────────────────────┘
```

### Limitations

- Max concurrent agents bounded by box RAM (each agent gets 4GB cgroup cap)
- No redundancy — if the box goes down, all sessions are lost
- Single point of failure for the control plane
- SQLite can become a bottleneck above ~50 active sessions/sec

### Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| `bootstrap.sh` fails at "Building Stronghold" | Missing build deps | `dnf install -y gcc gcc-c++ make cmake openssl-devel` |
| `/dev/sev not found` | Vultr plan doesn't have SEV-SNP | Use `--dev` for dev, or reprovision with SEV-SNP plan |
| `stronghold-gateway: failed` in systemd | Keys not initialized | `stronghold-gateway init --data-dir /var/lib/stronghold` |
| Port 8443 not reachable | Firewall blocking | `bash setup/firewall.sh`; check `firewall-cmd --list-all` |
| `curl https://localhost:8443` times out | Gateway bound to wrong address | Use `curl http://localhost:8443` (TLS not enabled — gap #1). Also check `Environment=STRONGHOLD_BIND` in systemd unit |
| Phone can't reach gateway | TLS cert issue (self-signed) | TLS is not enabled in alpha — use `http://`, or front the gateway with a Tailscale/WireGuard tunnel. Cert generation exists but is not wired into startup (gap #1) |

### Rollback

```bash
# Stop the gateway
systemctl stop stronghold-gateway

# Restore from a previous backup
bash setup/backup.sh --restore /var/lib/stronghold/backups/<latest>.tar.gz.age

# Or roll back the binary only (after upgrade.sh)
cp /var/lib/stronghold/upgrade-snapshots/<timestamp>/stronghold-gateway.prev \
   /usr/local/bin/stronghold-gateway
systemctl start stronghold-gateway
```

---

## Pattern 2: Multi-Box Fleet

Control plane on one box, multiple k3s workers. Suitable for 5+ concurrent
agents or when you need redundancy.

### Requirements

- 1 SEV-SNP-capable Vultr box for the control plane
- 1+ Vultr boxes for workers (SEV-SNP optional)
- Tailscale or WireGuard mesh between boxes (Tailscale strongly recommended)
- 16+ GB RAM on each worker (agents run here)

### Step-by-step

```bash
# ===== 1. Control plane box =====

# Provision SEV-SNP box, then bootstrap
ssh root@<cp-ip>
git clone https://github.com/pkhairkh/stronghold.git
cd stronghold
bash setup/bootstrap.sh

# Install Tailscale (required for multi-box)
bash setup/tailscale.sh --auth-key=tskey-...

# Note the control plane's Tailscale IP
CP_TS_IP=$(tailscale ip -4)
echo "Control plane Tailscale IP: $CP_TS_IP"

# Retrieve the k3s node token (control plane runs k3s server)
# Note: bootstrap.sh does NOT install k3s on the control plane.
# To enable multi-box, install k3s server:
curl -sfL https://get.k3s.io | sh -
cat /var/lib/rancher/k3s/server/node-token
# Save this token — workers need it to join

# ===== 2. Worker box(es) =====

# Provision a worker box (Rocky 9 or 10), then bootstrap as a worker
ssh root@<worker-ip>
git clone https://github.com/pkhairkh/stronghold.git
cd stronghold

# Install Tailscale first (workers join via Tailscale IP)
bash setup/tailscale.sh --auth-key=tskey-...

# Bootstrap the worker
bash setup/worker-bootstrap.sh \
    --host vultr-worker-1.fra1 \
    --token <k3s-node-token> \
    --server $CP_TS_IP \
    --tailscale

# ===== 3. Verify on the control plane =====

# k3s nodes registered
k3s kubectl get nodes

# Stronghold sees the worker
stronghold worker list
```

### Architecture

```
┌──────────────────────┐
│  Control Plane       │
│  (SEV-SNP Vultr)     │
│  ┌────────────────┐  │
│  │ stronghold     │  │
│  │ gateway (:8443)│  │
│  │ ntfy (:8090)   │  │
│  │ k3s server     │  │
│  └───────┬────────┘  │
└──────────┼───────────┘
           │ Tailscale mesh (encrypted WireGuard)
    ┌──────┼──────┐
    ▼      ▼      ▼
┌──────┐┌──────┐┌──────┐
│Worker││Worker││Worker│
│ k3s  ││ k3s  ││ k3s  │
│ntfy  ││ntfy  ││ntfy  │
│reg.  ││reg.  ││reg.  │
│node_ ││node_ ││node_ │
│ exp. ││ exp. ││ exp. │
└──┬───┘└──┬───┘└──┬───┘
   │       │       │
   ▼       ▼       ▼
 containerd pods (agent runtime)
```

### VPS Escalation

> ❌ **Stub — not yet implemented.** The gateway's VPS-escalation path returns
> `"stub-vps-id"` and `"0.0.0.0"` without calling the Vultr API. Dedicated /
> GPU orders will currently fail at the scheduling step. See gap #9.

For workloads needing more than any worker has (GPU, large memory), the
**planned** flow is:

```bash
# Agent requests dedicated VPS via gateway API
# NOTE: as of alpha this returns a stub — see gap #9
curl -X POST http://gateway:8443/agent/order \
  -H "Authorization: Bearer $AGENT_TOKEN" \
  -d '{
    "image": "stronghold/python-ml:2026.07",
    "compute": { "dedicated": true, "gpu": true, "memory_gb": 64 }
  }'
```

The gateway is intended to:

1. Call Vultr API to boot a fresh Rocky VPS with GPU
2. Cloud-init installs k3s worker, joins cluster (via Tailscale)
3. Pod is scheduled on the new VPS
4. On session end, VPS is destroyed, volumes snapshotted

None of these steps are implemented in the alpha release.

### Troubleshooting (multi-box)

| Symptom | Fix |
|---------|-----|
| Worker can't join k3s cluster | Verify Tailscale: `tailscale ping <cp-ip>`. Check 6443 open on CP Tailscale zone. |
| `k3s kubectl get nodes` shows `NotReady` | Worker kubelet not running: `systemctl status k3s-agent` on worker |
| Pods stuck in `Pending` | Check taints: `kubectl describe node`; check resource requests |
| ntfy pushes not reaching workers | ntfy auth: ensure worker has valid token; check `ntfy user list` on CP |

---

## Pattern 3: Community-Hosted

Stronghold as a service for multiple tenants. Each tenant has their own
credentials, quotas, and audit logs.

### Requirements

- Production-grade control plane (SEV-SNP, 16+ vCPU, 32+ GB RAM)
- 3+ boxes for HA control plane (etcd cluster)
- Multiple worker boxes
- External etcd for HA control plane (3 nodes)
- Backup strategy (S3-compatible object storage)
- Tailscale for inter-box mesh + perimeter

### Step-by-step

```bash
# 1. Provision 3 control-plane boxes with SEV-SNP
#    (only the active CP needs SEV-SNP; standbys can run without)

# 2. Bootstrap each CP box
for box in cp1 cp2 cp3; do
    ssh root@$box "bash setup/bootstrap.sh && bash setup/tailscale.sh --auth-key=tskey-..."
done

# 3. Install external etcd cluster (3 nodes)
#    See https://etcd.io/docs/v3.5/install/
#    Each etcd node runs on its own box or co-located with CP

# 4. Configure HA k3s control plane with external etcd
#    On cp1 (first server):
curl -sfL https://get.k3s.io | sh -s - server \
    --cluster-init \
    --etcd-servers=https://etcd1:2379,https://etcd2:2379,https://etcd3:2379

#    On cp2 and cp3 (join as servers):
curl -sfL https://get.k3s.io | sh -s - server \
    --server=https://cp1:6443 \
    --token=<k3s-token>

# 5. Add workers (see Pattern 2 step 2)

# 6. Create tenants on the active CP
stronghold tenant create --name "alice"
stronghold tenant create --name "bob"
stronghold tenant create --name "charlie"

# 7. Each tenant enrolls their own phone (separate setup passwords)
# 8. Each tenant mints their own agent tokens
stronghold agent-token mint --tenant alice --ttl 86400
stronghold agent-token mint --tenant bob   --ttl 86400

# 9. Configure automated backups to S3
echo 'BACKUP_ENCRYPTION_PASS=...' >> /etc/stronghold/backup.env
echo 'AWS_ACCESS_KEY_ID=...'      >> /etc/stronghold/backup.env
echo 'AWS_SECRET_ACCESS_KEY=...'  >> /etc/stronghold/backup.env
# Add to /etc/cron.d/stronghold-backup:
#   0 3 * * * root bash /root/stronghold/setup/backup.sh --to s3://my-bucket/stronghold/ --keep-days 30
```

### Multi-Tenant Isolation

| Layer | Isolation mechanism | Status (alpha) |
|-------|---------------------|----------------|
| Database | Separate `tenant_id` column on every row; row-level checks in code | ✅ Implemented |
| Audit log | Per-tenant hash chains; signatures verify per tenant | ✅ Writer implemented; ⚠️ `audit verify` only checks hash chain (gap #16) |
| Push notifications | Per-tenant ntfy topics with ACLs (see `setup/ntfy.yml`) | ✅ ntfy ACLs; ⚠️ payloads are plaintext (gap #4) |
| Pods | Kubernetes namespaces per tenant; network policies deny cross-tenant traffic | ❌ **Not yet implemented** (gaps #14, #15). All pods land in `default`; `tenant_id` is only a label. See [Roadmap](#roadmap). |
| Filesystem | Per-tenant volume mounts; no shared dirs | ✅ Implemented |
| Process | Separate PID namespace per pod | ✅ Implemented (k8s default) |
| Network | Per-tenant egress allowlists enforced by gateway | ❌ **Not yet implemented** (gap #15). No NetworkPolicy objects are created. |

### Billing (if you choose to monetize)

Stronghold does not include billing. If you want to charge tenants:

- Track resource usage per tenant (CPU-hours, GB-hours) via Prometheus labels
- Export usage data via `stronghold audit export --tenant <id> --format json`
- Integrate with Stripe or your billing system
- Use `stronghold tenant quota set --tenant <id> --cpu-hours 100` to enforce caps

### Troubleshooting (community-hosted)

| Symptom | Fix |
|---------|-----|
| Tenant A's pod can reach Tenant B's pod | Check network policies: `kubectl get netpol -A` |
| Audit log verify fails for one tenant | Key rotation incomplete: `stronghold keys rotate-audit` then re-verify |
| One CP node down, cluster still works | Expected — etcd quorum holds with 2/3 nodes |
| Backup fails to S3 | Check AWS creds in env; verify bucket policy allows s3:PutObject with SSE |

---

## Network Configuration

### Tailscale (Recommended)

```bash
# Install + configure
bash setup/tailscale.sh --auth-key=tskey-...

# Verify
tailscale status
tailscale ping <peer-hostname>
```

Tailscale provides:

- Encrypted mesh (WireGuard) between boxes
- ACLs to restrict which boxes can talk to which
- No public ports needed except 8443 (gateway) and 8090 (ntfy)
- Stable hostnames (`box.tailnet-name.ts.net`)

### WireGuard (Alternative)

If you can't use Tailscale (e.g. air-gapped):

```bash
dnf install -y wireguard-tools

# Generate keys on each box
wg genkey | tee privatekey | wg pubkey > publickey

# Configure peers in /etc/wireguard/wg0.conf
# See https://www.wireguard.com/quickstart/
```

### Firewall Rules

```bash
# Control plane: 8443 (gateway), 8090 (ntfy) public
#                 6443/10250/5000 on Tailscale only
bash setup/firewall.sh --role=control-plane

# Worker: 8090 (ntfy) public
#         6443/10250/5000/8472 on Tailscale only
bash setup/firewall.sh --role=worker
```

Verify from an external host:

```bash
nmap -p 8443,8090,6443,10250,5000 <box-ip>
# Expected: 8443, 8090 open; 6443, 10250, 5000 filtered
```

---

## Monitoring

### Quick start

```bash
# Just node_exporter (metrics endpoint)
bash setup/monitoring.sh

# Full stack: node_exporter + Prometheus + Grafana
bash setup/monitoring.sh --all
```

### Health Checks

```bash
# Gateway health (http://, not https:// — TLS not enabled, gap #1)
curl http://gateway:8443/agent/health

# ntfy health
curl http://gateway:8090/v1/health

# k3s health
kubectl get nodes

# Worker health
# NOTE: `worker health-check` is a stub (gap #10). Use kubectl instead.
# stronghold worker health-check --host vultr-worker-1
kubectl describe node vultr-worker-1

# Monitoring status
bash setup/monitoring.sh --status
```

### Logs

```bash
# Gateway logs (last 100 lines, follow)
journalctl -u stronghold-gateway -n 100 -f

# ntfy logs
journalctl -u ntfy -f

# k3s logs
journalctl -u k3s -f          # server (control plane)
journalctl -u k3s-agent -f    # agent (worker)
```

### Metrics

> ❌ **Not yet implemented.** There is no `/metrics` route on the gateway
> (gap #13). The Prometheus scrape target below will return 404 until the
> metrics route is added. The Grafana dashboard JSON shipped in
> `setup/monitoring/` is preserved for when this is implemented.

**Target state (planned):** the gateway exposes Prometheus metrics at
`http://<host>:8443/metrics` (HTTP, not HTTPS — see gap #1):

- `stronghold_sessions_active` — current active agent sessions
- `stronghold_approvals_pending` — pending phone approvals
- `stronghold_audit_entries_total` — total audit log entries
- `stronghold_sqlite_db_size_bytes` — SQLite DB file size
- Standard `process_*` and `http_*` Prometheus metrics

Import the pre-built dashboard (once `/metrics` exists):

```bash
# In Grafana: Dashboards → Import → Upload JSON file
/usr/share/stronghold/monitoring/stronghold-dashboard.json
```

---

## Backup & Restore

### Backup

```bash
# Local backup (encrypted)
BACKUP_ENCRYPTION_PASS='my-secret' \
    bash setup/backup.sh

# S3 backup (encrypted, with 30-day retention)
BACKUP_ENCRYPTION_PASS='my-secret' \
AWS_ACCESS_KEY_ID=... \
AWS_SECRET_ACCESS_KEY=... \
AWS_DEFAULT_REGION=us-east-1 \
    bash setup/backup.sh --to s3://my-bucket/stronghold/ --keep-days 30
```

### Restore (test on a fresh box!)

```bash
bash setup/backup.sh --restore /var/lib/stronghold/backups/stronghold-host-20260729T120000Z.tar.gz.age
```

### What's backed up

- SQLite DB (online snapshot via `.backup` — does not block writers)
- Audit keys (Ed25519 + ML-DSA-65)
- Push keys (X25519 + ML-KEM-768)
- Audit log files
- Config dir (TLS cert, ntfy config, server.yml) — *Note: TLS cert is not loaded by `serve()` in alpha (gap #1); files are backed up regardless.*
- Manifest with version metadata

### Automated backups

Add to `/etc/cron.d/stronghold-backup`:

```
0 3 * * * root /bin/bash /root/stronghold/setup/backup.sh --to s3://my-bucket/stronghold/ --keep-days 30
```

---

## Upgrades

### Check for new version

```bash
bash setup/upgrade.sh --check
```

### Upgrade

```bash
# Upgrade to latest release (verifies Ed25519 signature)
STRONGHOLD_SIGNING_KEY=<32-byte-hex-pubkey> \
    bash setup/upgrade.sh

# Upgrade to a specific version
bash setup/upgrade.sh --version v1.2.0

# Build from local source (dev)
bash setup/upgrade.sh --from-source

# Also rotate keys after upgrade
bash setup/upgrade.sh --rotate-keys
```

### What the upgrade does

1. Snapshots current binary, DB, and attestation
2. Downloads new binary (or builds from source)
3. Verifies Ed25519 signature against trusted key
4. Drains k3s node (if applicable)
5. Stops `stronghold-gateway`
6. Installs new binary
7. Runs DB migrations (`stronghold-gateway init`)
8. Re-attests SEV-SNP (records new measurement)
9. Optionally rotates audit + push keys
10. Restarts `stronghold-gateway`
11. Verifies audit log still verifies
12. Uncordons k3s node

### Rollback

```bash
# Stop, restore previous binary, restart
systemctl stop stronghold-gateway
cp /var/lib/stronghold/upgrade-snapshots/<timestamp>/stronghold-gateway.prev \
   /usr/local/bin/stronghold-gateway
systemctl start stronghold-gateway

# If keys were rotated, restore from backup:
bash setup/backup.sh --restore /var/lib/stronghold/backups/<pre-upgrade>.tar.gz.age
```

---

## Security Hardening

### SSH

```bash
# /etc/ssh/sshd_config
PasswordAuthentication no
PubkeyAuthentication yes
PermitRootLogin prohibit-password   # or "no" if you have a sudo user
Port 2222                            # non-default port (optional)

systemctl restart sshd
```

### Fail2ban

```bash
dnf install -y fail2ban
systemctl enable --now fail2ban
# Default config bans after 5 failed SSH attempts for 10 minutes
```

### Automatic Updates

```bash
dnf install -y dnf-automatic
systemctl enable --now dnf-automatic.timer
# Config: /etc/dnf/automatic.conf
#   upgrade_type = security
#   apply_updates = yes
```

### systemd Security Hardening

All Stronghold systemd units include:

- `NoNewPrivileges=true`
- `ProtectSystem=strict`
- `ProtectHome=true`
- `PrivateTmp=true`
- `ProtectKernelTunables/Modules/Logs=true`
- `ProtectControlGroups=true`
- `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`
- `RestrictNamespaces=true`
- `LockPersonality=true`
- `RestrictRealtime/SUIDSGID=true`
- `SystemCallFilter=@system-service`
- `SystemCallArchitectures=native`

Exceptions (intentional, documented in unit files):

- **Gateway**: `PrivateDevices=false` (needs `/dev/sev` for SEV-SNP attestation)
- **Gateway**: `MemoryDenyWriteExecute=false` (JIT in crypto libs)
- **k3s agent**: Most hardening relaxed (k3s needs broad privileges for container management)

Verify with:

```bash
systemd-analyze security stronghold-gateway.service
systemd-analyze security ntfy.service
# Target: exposure score < 5.0
```

---

## Quick Reference

| Script | Purpose | Idempotent |
|--------|---------|------------|
| `setup/bootstrap.sh` | Install control plane on fresh box | Yes |
| `setup/worker-bootstrap.sh` | Install k3s worker + ntfy + registry | Yes |
| `setup/firewall.sh` | Configure firewalld (public + Tailscale zones) | Yes |
| `setup/tailscale.sh` | Install/configure Tailscale | Yes |
| `setup/backup.sh` | Backup DB + keys + config (encrypted, S3) | Yes (new archive per run) |
| `setup/upgrade.sh` | Pull + verify + drain + restart + re-attest | Yes (no-op if same version) |
| `setup/monitoring.sh` | Install node_exporter + Prometheus + Grafana | Yes |

| File | Purpose |
|------|---------|
| `setup/ntfy.yml` | ntfy server config (ACL'd, auth required, attachments disabled) |
| `setup/systemd/stronghold-gateway.service` | Gateway systemd unit (hardened, /dev/sev allowed) |
| `setup/systemd/ntfy.service` | ntfy systemd unit (fully hardened) |
| `setup/systemd/k3s-worker.service` | k3s agent systemd unit (relaxed hardening for container mgmt) |

---

## Roadmap

The following deployment-pattern features are **planned but not yet implemented**
as of `0.9.0-alpha`. They were previously described in this document as if
already shipping; they have been moved here for accuracy.

- **TLS termination in `serve()`.** Wire the existing `crypto/tls.rs` config
  into the axum server. The self-signed cert generator (`rcgen`) already
  exists; it just is not loaded on startup. See gap #1.
- **Per-tenant Kubernetes namespaces.** Currently all pods land in `default`;
  `tenant_id` is only a label. Planned: one namespace per tenant, enforced at
  the namespace boundary. See gap #14.
- **Per-tenant NetworkPolicy objects.** No `NetworkPolicy` objects are
  created today. Planned: default-deny egress per tenant with an allowlist
  (github.com, crates.io, etc.). See gap #15.
- **Prometheus `/metrics` route.** No `/metrics` route exists today. Planned:
  expose `stronghold_sessions_active`, `stronghold_approvals_pending`,
  `stronghold_audit_entries_total`, `stronghold_sqlite_db_size_bytes`. See
  gap #13.
- **VPS escalation via Vultr API.** Replace the stub with real cloud-init +
  k3s-agent join. See gap #9.
- **`worker add` / `worker list`.** Implement real SSH/cloud-init provisioning
  and a real worker registry. See gap #10.
- **SEV-SNP golden integration tests on real hardware.** Blocked on
  provisioning a Vultr SEV-SNP box. See gap #18.

See [CHANGELOG.md → Known Open Gaps](../CHANGELOG.md#known-open-gaps-alpha-scope--advertised-but-not-enforced-in-the-running-gateway)
for the full alpha-gap list.

