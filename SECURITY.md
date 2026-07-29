# Security Policy

> 🟡 **Beta — not recommended for production without further testing.**
>
> Stronghold `0.10.x-beta` has closed all 18 previously-tracked alpha gaps.
> TLS termination, WebAuthn signature verification, PTY `connect_token` auth,
> E2E-encrypted push notifications, anomaly scanning, quorum enforcement,
> real SSE approval events, real audit streaming, full `audit verify`
> signature checks, Prometheus `/metrics`, real worker listing, real image
> builds, global rate limiting, request tracing, and the `--dev` flag are
> all wired into the running gateway. See the status indicators below.
>
> Remaining limitations are documented in [Known Limitations](README.md#known-limitations).
> They are either hardware-blocked (SEV-SNP on real silicon, FIDO PQC
> authenticators), out of scope for the current multi-tenancy model, or
> deliberate stubs deferred to the v1.0 RC.

## Supported Versions

| Version         | Supported          |
|-----------------|--------------------|
| 0.10.x-beta     | :warning: (beta — security fixes only; not recommended for production without further testing) |
| 0.9.x-alpha     | :x: (superseded — see migration notes in [CHANGELOG.md](CHANGELOG.md)) |
| < 0.9.x         | :x: (unsupported scaffold) |

## Reporting a Vulnerability

If you discover a security vulnerability in Stronghold, please **do not** open a public GitHub issue.

Instead, please email **security@stronghold.dev** with:
1. A description of the vulnerability
2. Steps to reproduce
3. Potential impact
4. Any suggested fixes (optional)

### Response Timeline

- **Acknowledgment**: Within 48 hours
- **Initial Assessment**: Within 7 days
- **Fix or Mitigation**: Within 90 days (severity-dependent)
- **Public Disclosure**: After fix is released, coordinated with reporter

## Security Architecture

Stronghold's security model is documented in:
- [Threat Model](docs/THREAT_MODEL.md)
- [SEV-SNP Attestation](docs/SEV_SNP.md)
- [ADRs](docs/adr/)

### Key Security Properties

Legend: ✅ = works · ⚠️ = partial / hardware-blocked · ❌ = not implemented

| # | Property | Status | Notes |
|---|----------|--------|-------|
| 1 | **Post-Quantum Transport** (TLS 1.3 + X25519MLKEM768 hybrid) | ✅ | Gateway serves real HTTPS via `axum_server::bind_rustls()` with the hybrid PQ key exchange. Self-signed cert auto-generated on first boot via `rcgen` 0.14 if missing. |
| 2 | **Dual-Signed Audit** (Ed25519 + ML-DSA-65 on every entry) | ✅ | Real `ml-dsa` 0.1.1 signatures; hash-chained; offline-verifiable. The `audit verify` CLI now checks the hash chain **and** both signature types. |
| 3 | **SEV-SNP Confidential Computing** | ⚠️ | `sev` crate wired in with real ioctl calls; key sealing + attestation report generation implemented and tested with software keys. **Never tested on real SEV-SNP hardware** — dev box lacks `/dev/sev-guest`. Measurement registry is a placeholder. Hardware-blocked; revisit when a Vultr SEV-SNP box is provisioned. |
| 4 | **WebAuthn Session Approval** | ✅ | Phishing-resistant challenge generation, assertion metadata verification, **and real ECDSA P-256 signature verification** against the stored credential public key. Approvals are now proofs of possession. |
| 5 | **Quorum for Destructive Ops** | ✅ | Destructive commands are blocked, a `pending_sessions` row is created, the proxy polls for approval, and executes only on approval. |
| 6 | **No External Providers for Content** | ✅ | ntfy is self-hosted; APNs/FCM are wake-up only. |
| 7 | **Fail Closed** | ✅ | Every failure mode denies rather than allows. The PTY proxy fails closed: missing `connect_token` (401), missing quorum (block), and unhandled anomaly (block) all deny the session. |
| 8 | **PTY `connect_token` verification** | ✅ | Token verified against its SHA-256 hash stored in the `machines` table. Missing/wrong token → 401. |
| 9 | **E2E-encrypted push notifications** | ✅ | All 5 production push functions use `send_encrypted_or_fallback()`. Payloads sealed with X25519 + ML-KEM-768 hybrid KEM → HKDF-256 → AES-256-GCM when the phone has enrolled keys. |
| 10 | **Anomaly scanning** | ✅ | Wired into the PTY proxy. Detects `curl`/`wget`/`scp`, `rm -rf`, `sudo`, `ssh`; writes audit entries. |
| 11 | **Audit streaming WebSocket** | ✅ | Real `audit_stream()` long-polls the DB and streams JSON audit entries to authorised clients. |
| 12 | **SSE approval events** | ✅ | `pending_approval_stream()` polls the DB every 500 ms and yields real `approval_request` events. |
| 13 | **Prometheus metrics** | ✅ | `GET /metrics` returns Prometheus text: `sessions_active`, `approvals_pending`, `audit_entries_total`. |
| 14 | **Real worker listing** | ✅ | `kube::Api::<Node>::list()` with capacity parsing (allocatable CPU/memory). |
| 15 | **Real image build** | ✅ | `podman build` + `podman inspect` → real digest. |
| 16 | **Rate limiting** | ✅ | Global concurrency limit (cap 100; 503 on overflow). |
| 17 | **Request tracing** | ✅ | `TraceLayer` on all routes. |
| 18 | **`--dev` flag** | ✅ | Properly threads through; skips the SEV-SNP availability check. |

### Other known limitations (beta scope)

- ⚠️ **WebAuthn PQC** — FIDO authenticators do not yet ship with post-quantum algorithms (~2027 expected). Hardware limitation; not fixable in software. Session TTLs are hours, so a quantum break in 10 years gets nothing useful.
- ⚠️ **SEV-SNP on real hardware** — dev box lacks `/dev/sev`. Code compiles and key sealing is tested with software keys. Hardware-blocked; revisit when a Vultr SEV-SNP box is provisioned.
- ⚠️ **Per-token rate limiting** — only a global concurrency limit (cap 100, 503 on overflow) is enforced. There is no per-token bucket.
- ⚠️ **Per-tenant Kubernetes namespaces** — all pods land in `default`; `tenant_id` is a pod label, not a namespace boundary.
- ❌ **Per-tenant NetworkPolicy objects** — not created; cross-tenant pod traffic is not denied at the network layer.
- ❌ **VPS escalation** — `machines/escalation.rs` still returns a fake VPS ID.
- ❌ **Image push / image pull** — still stubs (no registry interactions).
- ⚠️ **Anomaly push to phone** — `push_anomaly()` is defined but never called. Anomalies are written to the audit log only; the phone is not pushed.
- ⚠️ **Quorum push to phone** — quorum requests land in `pending_sessions` but no ntfy push fires. The phone polls the SSE stream instead.

See [CHANGELOG.md](CHANGELOG.md#known-issues) for the exhaustive list.

## Security Considerations for Operators

If you are running Stronghold (beta — not recommended for production without further testing):

1. **Verify SEV-SNP Measurement**: Before enrolling credentials, verify the `SEV_SNP_MEASUREMENT` matches the published measurement in `docs/MEASUREMENTS/`. *Note: `docs/MEASUREMENTS/v1.0.txt` is currently an all-zero placeholder; SEV-SNP has not been tested on real hardware. The `--dev` flag skips this check on boxes without `/dev/sev`.*
2. **Rotate Keys Regularly**: Use `stronghold keys rotate-audit` periodically.
3. **Review Audit Logs**: Run `stronghold audit verify` regularly to detect tampering. The verifier now checks the hash chain, Ed25519 signatures, and ML-DSA-65 signatures.
4. **Keep Backups**: Use `stronghold backup` to encrypt and store key material off-box.
5. **Harden SSH**: Disable password auth, use ed25519 keys only, install fail2ban.
6. **Network Isolation**: Use Tailscale for worker-to-control-plane communication. The gateway now serves real HTTPS (TLS 1.3 + X25519MLKEM768), but Tailscale adds defence-in-depth and simplifies multi-box mesh routing.
7. **Port 8443 is now a real HTTPS endpoint** — the gateway terminates TLS with a self-signed cert on first boot. Phones and agents connect via `https://` (and `wss://` for WebSockets). Pin the gateway's self-signed cert or front it with a Tailscale/WireGuard tunnel for additional trust anchoring.
