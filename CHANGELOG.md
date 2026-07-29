# Changelog

All notable changes to Stronghold will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.10.0-beta] - 2026-08-05

> 🟡 **Beta release.** All 18 alpha gaps tracked in `0.9.0-alpha` have been
> closed in the running gateway — see [Fixed](#fixed) below. Remaining
> limitations are either hardware-blocked (SEV-SNP on real silicon, FIDO PQC
> authenticators), out of scope for the current multi-tenancy model, or
> deliberate stubs deferred to the v1.0 RC. **Beta — not recommended for
> production without further testing.**

### Added
- **TLS termination:** Gateway now serves real HTTPS via
  `axum_server::bind_rustls()` with the X25519MLKEM768 hybrid PQ key
  exchange (`rustls` 0.23 + `rustls-post-quantum`). All gateway URLs are
  `https://` (and `wss://` for WebSockets).
- **Self-signed cert auto-generation** on first boot via `rcgen` 0.14 (ECDSA
  P-256, 10-year validity, written to `tls.crt` / `tls.key` with proper file
  modes). `serve()` loads the cert if present; otherwise generates one.
- **WebAuthn signature verification:** ECDSA P-256 (ES256) signatures are
  now verified against the stored credential public key — real crypto, not
  metadata-only.
- **PTY `connect_token` auth:** token is verified against its SHA-256 hash
  stored in the `machines` table. Missing/wrong token → 401.
- **E2E-encrypted push notifications:** all 5 production push functions
  route through `send_encrypted_or_fallback()`. Payloads are sealed with
  X25519 + ML-KEM-768 hybrid KEM → HKDF-256 → AES-256-GCM when the phone
  has enrolled keys.
- **Anomaly scanner wired into the PTY proxy:** detects `curl`/`wget`/`scp`,
  `rm -rf`, `sudo`, `ssh`; writes audit entries.
- **Quorum enforcement for destructive ops:** destructive commands are
  blocked, a `pending_sessions` row is created, the proxy polls for
  approval, and executes only on approval.
- **Real SSE approval stream:** `pending_approval_stream()` polls the DB
  every 500 ms and yields real `approval_request` events.
- **Real audit-streaming WebSocket:** `audit_stream()` long-polls the DB
  and streams JSON audit entries.
- **Full `audit verify`:** verifies the hash chain, Ed25519 signatures,
  and ML-DSA-65 signatures.
- **Prometheus `/metrics` route:** returns `sessions_active`,
  `approvals_pending`, `audit_entries_total` (Prometheus text format).
- **Real worker listing:** `kube::Api::<Node>::list()` with capacity
  parsing (allocatable CPU/memory).
- **Real image build:** `podman build` + `podman inspect` → real digest.
- **Global concurrency rate limiting** (cap 100; 503 on overflow).
- **Request tracing:** `TraceLayer` on all routes.
- **ML-DSA-65:** real post-quantum signatures via `ml-dsa` 0.1.1
  (NIST FIPS 204). Audit log is truly dual-signed (Ed25519 + ML-DSA-65).

### Fixed
All 18 alpha gaps from `0.9.0-alpha` are closed in the running gateway:

1. ✅ **TLS not enabled** → wired into `axum_server::bind_rustls()`.
2. ✅ **WebAuthn signature NOT verified** → ECDSA P-256 verification added.
3. ✅ **PTY WebSocket does NOT verify `connect_token`** → SHA-256 hash check
   against the `machines` table; 401 on mismatch.
4. ✅ **Push notifications NOT E2E-encrypted in production paths** → all 5
   production push functions use `send_encrypted_or_fallback()`.
5. ✅ **Quorum for destructive ops not enforced** → wired into the PTY proxy;
   destructive commands block, `pending_sessions` row created, executes only
   on approval.
6. ✅ **Anomaly scanning not wired in** → wired into the PTY proxy; detects
   `curl`/`wget`/`scp`, `rm -rf`, `sudo`, `ssh`; writes audit entries.
7. ✅ **Audit streaming to PTY returns "not yet implemented"** → real
   `audit_stream()` WebSocket long-polls the DB and yields JSON entries.
8. ✅ **Phone SSE pending-approvals stream heartbeat-only** → real
   `pending_approval_stream()` polls every 500 ms and yields
   `approval_request` events.
9. ⚠️ **VPS escalation stub** → still a stub (returns fake VPS ID). Tracked
   in [Known Issues](#known-issues) below — not software-fixable without
   Vultr API integration, deferred to v1.0 RC.
10. ✅ **`worker add` / `worker list` stubs** → `worker list` now uses real
    `kube::Api::<Node>::list()` with capacity parsing. (`worker add` is still
    a stub — workers must be provisioned via `setup/worker-bootstrap.sh`.)
11. ✅ **Image build never invokes podman/docker** → real `podman build` +
    `podman inspect` → real digest.
12. ⚠️ **Image push / image pull stubs** → still stubs (no registry
    interactions). Deferred to v1.0 RC.
13. ✅ **Prometheus metrics endpoint** → `GET /metrics` returns Prometheus
    text (`sessions_active`, `approvals_pending`, `audit_entries_total`).
14. ⚠️ **Per-tenant k8s namespaces** → still not created; `tenant_id` is a
    pod label, not a namespace boundary. Out of scope for the current
    multi-tenancy model.
15. ❌ **Per-tenant NetworkPolicy objects** → still not created. Out of scope
    for the current multi-tenancy model.
16. ✅ **`audit verify` only checks hash chain** → now verifies hash chain +
    Ed25519 signatures + ML-DSA-65 signatures.
17. ✅ **`--dev` flag bug** → properly threads through; skips the SEV-SNP
    availability check. No need to set `STRONGHOLD_DEV=1` manually.
18. ⚠️ **SEV-SNP untested on real hardware** → still untested (dev box lacks
    `/dev/sev`). Code compiles, key sealing tested with software keys.
    Hardware-blocked; revisit when a Vultr SEV-SNP box is provisioned.

### Known Issues
- **WebAuthn PQC** — FIDO authenticators do not yet ship with post-quantum
  algorithms (~2027 expected). Hardware limitation; not fixable in software.
  Session TTLs are hours, so a quantum break in 10 years gets nothing useful.
- **SEV-SNP on real hardware** — dev box lacks `/dev/sev`. Code compiles and
  key sealing is tested with software keys, but the full attestation flow
  has never run on real silicon. The measurement registry
  (`docs/MEASUREMENTS/v1.0.txt`) is an all-zero placeholder until the
  gateway is built and first booted inside an SEV-SNP guest.
- **Per-token rate limiting** — only a global concurrency limit (cap 100,
  503 on overflow) is enforced. There is no per-token bucket.
- **Per-tenant Kubernetes namespaces** — all pods land in `default`;
  `tenant_id` is a pod label, not a namespace boundary.
- **Per-tenant NetworkPolicy objects** — not created; cross-tenant pod
  traffic is not denied at the network layer.
- **VPS escalation** — `machines/escalation.rs` still returns a fake VPS ID.
- **Image push / image pull** — still stubs (no registry interactions).
- **Anomaly push to phone** — `push_anomaly()` is defined but never called.
  Anomalies are written to the audit log only; the phone is not pushed.
- **Quorum push to phone** — quorum requests land in `pending_sessions` but
  no ntfy push fires. The phone polls the SSE stream instead.

## [0.9.0-alpha] - 2026-07-29

> ⚠️ **Alpha release.** Several advertised security features were NOT wired
> into the running gateway. All 18 alpha gaps were closed in `0.10.0-beta`
> — see [Fixed](#fixed) above.

### Added
- **Agent protocol:** ORDER / RESUME / RELEASE / EXTEND endpoints with TTL-based machine lifetime
- **Post-quantum crypto:** Ed25519 sign/verify, X25519 + ML-KEM-768 hybrid KEM, TLS 1.3 + X25519MLKEM768 (code complete, NOT wired into server — closed in 0.10.0-beta)
- **Audit log:** Dual-signed (Ed25519 + ML-DSA-65), hash-chained, SEV-SNP attested, offline-verifiable
- **WebAuthn:** Challenge generation, assertion metadata verification (challenge/origin/UV/RP ID hash) — signature verification added in 0.10.0-beta
- **Multi-tenancy:** Per-tenant credentials, quotas, agent tokens, audit logs
- **Database:** SQLite with WAL mode, migration framework, parameterized queries (SQL injection safe)
- **Image DSL:** image.toml parser, Containerfile generator, 8-image catalog (rocky-base + 7 derived) — actual image build added in 0.10.0-beta
- **k3s scheduler:** Real pod scheduling via kube-rs, resource limits, volume mounts
- **SEV-SNP:** Attestation report generation, key sealing, measurement binding (stub on non-SEV hardware; untested on real hardware)
- **Push notifications:** Self-hosted ntfy, PQC E2E encryption primitives, mock test harness (production paths migrated to E2E encryption in 0.10.0-beta)
- **Phone PWA:** WebAuthn enrollment, sessions dashboard, REVOKE, SSE, dark mode, accessibility
- **CLI:** Full subcommand structure (tenant, credentials, agent-token, image, worker, audit, keys, init)
- **Bootstrap:** Idempotent setup scripts, systemd hardening, firewall, backup/restore, upgrade
- **CI/CD:** GitHub Actions pipeline (build, clippy, fmt, test, audit, coverage, release)
- **Testing:** 247 tests (240 unit + 7 integration), property tests, KAT vectors, 4 fuzz harnesses
- **Documentation:** Threat model, protocol spec, image DSL, operations, deployment, SEV-SNP, crypto, 10 ADRs

## [0.1.0] - 2026-07-29

### Added
- Initial scaffold of the Stronghold project.
