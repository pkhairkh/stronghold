# Changelog

All notable changes to Stronghold will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0-alpha] - 2026-07-29

> ⚠️ **Alpha release.** Several advertised security features are NOT wired into
> the running gateway. See "Known Open Gaps" below. **DO NOT DEPLOY IN
> PRODUCTION.**

### Added
- **Agent protocol:** ORDER / RESUME / RELEASE / EXTEND endpoints with TTL-based machine lifetime
- **Post-quantum crypto:** Ed25519 sign/verify, X25519 + ML-KEM-768 hybrid KEM, TLS 1.3 + X25519MLKEM768 (code complete, NOT wired into server — see gap #1)
- **Audit log:** Dual-signed (Ed25519 + ML-DSA-65), hash-chained, SEV-SNP attested, offline-verifiable
- **WebAuthn:** Challenge generation, assertion metadata verification (challenge/origin/UV/RP ID hash) — signature verification NOT implemented (gap #2)
- **Multi-tenancy:** Per-tenant credentials, quotas, agent tokens, audit logs
- **Database:** SQLite with WAL mode, migration framework, parameterized queries (SQL injection safe)
- **Image DSL:** image.toml parser, Containerfile generator, 8-image catalog (rocky-base + 7 derived) — actual image build NOT invoked (gap #11)
- **k3s scheduler:** Real pod scheduling via kube-rs, resource limits, volume mounts
- **SEV-SNP:** Attestation report generation, key sealing, measurement binding (stub on non-SEV hardware; untested on real hardware — gap #18)
- **Push notifications:** Self-hosted ntfy, PQC E2E encryption primitives, mock test harness (production paths send plaintext — gap #4)
- **Phone PWA:** WebAuthn enrollment, sessions dashboard, REVOKE, SSE, dark mode, accessibility
- **CLI:** Full subcommand structure (tenant, credentials, agent-token, image, worker, audit, keys, init)
- **Bootstrap:** Idempotent setup scripts, systemd hardening, firewall, backup/restore, upgrade
- **CI/CD:** GitHub Actions pipeline (build, clippy, fmt, test, audit, coverage, release)
- **Testing:** 247 tests (240 unit + 7 integration), property tests, KAT vectors, 4 fuzz harnesses
- **Documentation:** Threat model, protocol spec, image DSL, operations, deployment, SEV-SNP, crypto, 10 ADRs

### Known Issues
- WebAuthn PQC gap (FIDO authenticators not deployed yet — hardware limitation, ~2027)
- SEV-SNP untested on real hardware (dev box lacks /dev/sev; software key sealing tested)

### Known Open Gaps (alpha scope — advertised but NOT enforced in the running gateway)

Legend: ❌ = not implemented · ⚠️ = code exists but is not wired in · ✅ = works

1. ⚠️ **TLS not enabled** — `main.rs::serve()` binds plain TCP and serves HTTP.
   `crypto/tls.rs` builds a TLS config that is then discarded
   (`let _tls_config = ...`).
2. ⚠️ **WebAuthn signature NOT verified** — only assertion metadata is checked
   (challenge / origin / UV flag / RP ID hash). The cryptographic signature is
   never validated. Anyone who can craft a syntactically valid assertion blob
   can approve any session.
3. ❌ **PTY WebSocket does NOT verify `connect_token`** — anyone with the WS
   URL can attach to any session.
4. ⚠️ **Push notifications NOT E2E-encrypted in production** — only the
   test-only `send_encrypted_notification_to()` encrypts; all production
   paths send plaintext.
5. ❌ **Quorum for destructive ops** — data structures exist in
   `sessions/scopes.rs` but nothing calls them. The PTY proxy does not scan
   commands or block.
6. ❌ **Anomaly scanning** — `anomaly/mod.rs` has a working scanner but the
   PTY proxy never instantiates or calls it.
7. ❌ **Audit streaming to PTY** — `routes/pty.rs::audit_stream()` sends
   "not yet implemented" and returns.
8. ⚠️ **Phone SSE pending-approvals stream** —
   `sessions/manager.rs::pending_approval_stream()` only emits heartbeats
   every 30s. The phone never receives approval requests via SSE.
9. ❌ **VPS escalation** — stub (returns `"stub-vps-id"`, `"0.0.0.0"`).
10. ❌ **`worker add` / `worker list`** — `add()` does nothing; `list()`
    returns an empty `Vec`.
11. ❌ **Image build** — `images/builder.rs` generates a Containerfile but
    never calls podman/docker.
12. ❌ **Image push / image pull** — stubs.
13. ❌ **Prometheus metrics** — no `/metrics` route exists.
14. ❌ **Per-tenant k8s namespaces** — all pods land in `default`;
    `tenant_id` is only a label.
15. ❌ **Per-tenant NetworkPolicy objects** — never created.
16. ⚠️ **`audit verify` CLI** — only checks the hash chain; signature
    verification is TODO.
17. ❌ **`--dev` flag bug** — sets a struct field, not the `STRONGHOLD_DEV`
    env var, so it does not actually bypass the SEV-SNP check in `serve()`.
    Use `STRONGHOLD_DEV=1` instead.
18. ⚠️ **SEV-SNP untested on real hardware** — the `sev` crate is wired in,
    but no SEV-SNP-capable box has been provisioned yet. The measurement
    registry file is a placeholder.

### Post-v1.0.0 Gap Fixes (subset closed; remaining gaps above)
- ✅ **ML-DSA-65**: Real post-quantum signatures via `ml-dsa` 0.1.1 crate
  (was deferred). Audit log is truly dual-signed (Ed25519 + ML-DSA-65).
- ✅ **PTY proxy data path**: Real k8s exec via kube-rs WebSocket with
  stdin/stdout/stderr/tty (was stub). *Note: connect_token verification is
  still missing — see gap #3.*
- ⚠️ **Self-signed cert**: `generate_self_signed_cert()` exists via `rcgen`
  0.14 (ECDSA P-256, 10-year validity, writes tls.crt + tls.key) — **but it
  is not wired into server startup.** `serve()` does not load the cert. See
  gap #1.

## [0.1.0] - 2026-07-29

### Added
- Initial scaffold of the Stronghold project.
