# Stronghold Operations Guide

> 🟡 **Beta release.** Most CLI subcommands and audit-verification steps
> described below are now implemented. Remaining limitations are noted
> inline with ⚠️ / ❌ markers. See the [Known Limitations](../README.md#known-limitations)
> list in the README for the full set of beta gaps.

## Tenant Management

### Create a tenant

```bash
stronghold tenant create --name "alice"
```

Output:
```
Tenant ID: tenant_01HXYZ...
Setup password (save this — it will not be shown again):
  AbCdEf123...
Enrollment URL: https://gateway:8443/setup?tenant=tenant_01HXYZ...
SEV-SNP measurement: sha256:abc123...
```

### List tenants

```bash
stronghold tenant list
```

### Set quotas

```bash
stronghold tenant quota set --tenant tenant_01HXYZ... \
  --max-concurrent-machines 5 \
  --max-cpu-per-machine 8 \
  --max-memory-gb-per-machine 16
```

---

## Credential Management

### Enroll a new phone

1. Open the enrollment URL in the phone's native browser
2. Enter the setup password
3. Verify the SEV-SNP measurement matches `docs/MEASUREMENTS/v1.0.txt`
4. Complete Face ID enrollment

### Enroll a backup credential

```bash
stronghold credentials enroll --tenant tenant_01HXYZ...
```

This generates a new enrollment URL. Open it on the second device (phone, YubiKey, laptop).

### List credentials

```bash
stronghold credentials list --tenant tenant_01HXYZ...
```

### Revoke a credential (lost phone)

```bash
stronghold credentials revoke --id cred_01HXYZ...
```

The credential is immediately revoked. Any pending approvals requiring that credential are auto-denied.

### Lost-all-credentials recovery

If you lose all enrolled credentials:
1. Boot the gateway with the `--recovery` flag
2. Enter the setup password (printed at install time, stored offline)
3. Enroll a new credential
4. Reboot normally

---

## Agent Token Management

### Mint an agent token

```bash
stronghold agent-token mint --tenant tenant_01HXYZ... --scope default --ttl 86400
```

Output:
```
Agent token (save this — it will not be shown again):
  stronghold_agent_abc123...
```

### List active tokens

```bash
stronghold agent-token list --tenant tenant_01HXYZ...
```

### Revoke a token

```bash
stronghold agent-token revoke --token stronghold_agent_abc123...
```

---

## Key Rotation

### Rotate audit keys

```bash
stronghold keys rotate-audit
```

This:
1. Generates a new Ed25519 + ML-DSA-65 keypair
2. Seals them to the current SEV-SNP measurement
3. Signs a `key_rotation` audit entry with the OLD keys
4. All subsequent entries are signed with the NEW keys
5. OLD keys are retained read-only for verifying historical entries

### Rotate push keys

```bash
stronghold keys rotate-push
```

This:
1. Generates a new X25519 + ML-KEM-768 keypair
2. All phones must re-enroll (old push keys are invalidated)

---

## Audit Log

### Verify the audit log

```bash
stronghold audit verify --tenant tenant_01HXYZ...
```

Output (current beta behavior — full verification):
```
Verifying audit log for tenant tenant_01HXYZ...
  Entries: 1,247
  Hash chain: OK
  Ed25519 signatures: OK
  ML-DSA-65 signatures: OK
  SEV-SNP attestation: OK (when gateway was in TEE mode)
```

> ✅ The verifier now checks the hash chain, Ed25519 signatures, ML-DSA-65
> signatures, and SEV-SNP attestation-report hashes. The audit log writer
> has always dual-signed every entry; the verifier now matches the writer's
> guarantees.

### Export audit log

```bash
# JSON format
stronghold audit export --tenant tenant_01HXYZ... \
  --from 2026-07-01 --to 2026-07-31 --format json > audit-july.json

# Text format
stronghold audit export --tenant tenant_01HXYZ... \
  --machine mach_01HXYZ... --format text
```

---

## Backup and Restore

### Backup

```bash
stronghold backup --to s3://my-backup-bucket/stronghold-$(date +%Y%m%d).tar.gz
```

This backs up:
- SQLite databases (audit logs, credentials, tenants)
- Key files (encrypted at rest with tenant password)
- Image catalog (image.toml files)
- Tenant configs (scopes.toml, anomaly.toml, quotas)
- TLS certificate and private key (`tls.crt` / `tls.key`, now loaded by `serve()`)

### Restore

```bash
stronghold restore --from s3://my-backup-bucket/stronghold-20260729.tar.gz
```

Keys can only be unsealed on the same SEV-SNP measurement (or after a key-rotation ceremony).

---

## Worker Management

### Add a worker

> ⚠️ **Not yet implemented — no-op.** `stronghold worker add` is still a
> stub: it parses the CLI args and returns successfully, but performs no
> SSH, no cloud-init, and no k3s installation. Workers must be provisioned
> manually via `setup/worker-bootstrap.sh` until this is implemented.

```bash
stronghold worker add --host vultr-worker-4.fra1 --token <k3s-token>
```

(Documentation of intended behavior: this would SSH (or use Vultr cloud-init)
to the worker and install k3s.)

### List workers

```bash
stronghold worker list
```

Output (beta — real listing via `kube::Api::<Node>::list()` with capacity parsing):
```
vultr-worker-1.fra1   8 cpu / 16GB / 200GB   sev-snp: yes   3 pods active
vultr-worker-2.fra1   8 cpu / 16GB / 200GB   sev-snp: no    1 pod active
vultr-worker-3.ams1   4 cpu / 8GB  / 100GB   sev-snp: yes   0 pods active
```

### Remove a worker

> ⚠️ **Not yet implemented.** `stronghold worker remove` is still a stub.

```bash
stronghold worker remove --host vultr-worker-3.ams1
```

(Documentation of intended behavior: pods are drained to other workers
before removal.)

---

## Session Management

### View active sessions

Open the gateway URL in your phone browser. The dashboard shows all active sessions for your tenant.

> ✅ **Phone SSE is live.**
> `sessions/manager.rs::pending_approval_stream()` polls the DB every 500 ms
> and yields real `approval_request` events. Approval requests reach the
> phone via SSE in addition to ntfy push notifications (which are now
> E2E-encrypted when the phone has enrolled keys).

### Revoke a session

Tap **REVOKE** on any session card in the phone dashboard. The session is killed within 500ms.

### Extend a session

The agent calls `POST /agent/extend`. This triggers a new phone approval. On approval, the TTL is extended.

---

## Upgrading

### Upgrade the gateway

```bash
stronghold upgrade
```

This:
1. Pulls the latest binary from GitHub releases
2. Verifies the binary signature (cosign / GPG)
3. Drains the control plane, restarts the gateway
4. SEV-SNP re-attests with the new measurement
5. Audit keys are re-sealed to the new measurement (signed `key_rotation` entry)
6. Runs database migrations if needed

### Verify the new measurement

After upgrade, verify the new SEV-SNP measurement matches the published measurement for the new version:

```bash
curl https://gateway:8443/attestation | jq .measurement
# Compare with docs/MEASUREMENTS/v1.1.txt
```

> ⚠️ The gateway now serves real HTTPS (TLS 1.3 + X25519MLKEM768 hybrid).
> Use `https://`, not `http://`. Note `docs/MEASUREMENTS/v1.0.txt` is
> currently an all-zero placeholder; SEV-SNP has not been tested on real
> hardware.

---

## Troubleshooting

### Gateway won't start

```bash
# Check SEV-SNP
ls -la /dev/sev

# Check logs
journalctl -u stronghold-gateway -f

# Run in dev mode (skips SEV-SNP)
stronghold-gateway serve --dev
```

> ✅ The `--dev` flag now properly threads through and skips the SEV-SNP
> availability check — no need to set `STRONGHOLD_DEV=1` manually.

### Phone can't connect

```bash
# Check firewall
firewall-cmd --list-ports

# Check TLS — gateway now terminates TLS with a self-signed cert on first boot.
openssl s_client -connect gateway:8443
# (self-signed cert → expected verification warning; pin it on the phone)

# Check ntfy
curl https://gateway:8090/v1/health
```

### Agent can't ORDER

```bash
# Check agent token
stronghold agent-token list --tenant <id>

# Check tenant quota
stronghold tenant get --id <id>

# Check gateway health (gateway now serves HTTPS — use https://)
curl https://gateway:8443/agent/health
```

### Audit verification fails

```bash
# Run verification with verbose output
stronghold audit verify --tenant <id> --verbose

# Check for hash chain breaks
sqlite3 /var/lib/stronghold/audit/<tenant>.db \
  "SELECT seq, hash, prev_hash FROM audit_entries ORDER BY seq DESC LIMIT 10"
```

> ✅ `audit verify` now checks the hash chain, Ed25519 signatures, and
> ML-DSA-65 signatures. A failure indicates either a tampered entry, a
> corrupted signature, or a key-rotation mismatch.
