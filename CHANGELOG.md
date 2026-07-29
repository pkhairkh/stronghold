# Changelog

All notable changes to Stronghold will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-07-29

### Added
- **Agent protocol:** ORDER / RESUME / RELEASE / EXTEND endpoints with TTL-based machine lifetime
- **Post-quantum crypto:** Ed25519 sign/verify, X25519 + ML-KEM-768 hybrid KEM, TLS 1.3 + X25519MLKEM768
- **Audit log:** Dual-signed (Ed25519), hash-chained, SEV-SNP attested, offline-verifiable
- **WebAuthn:** Challenge generation, assertion metadata verification (challenge/origin/UV/RP ID hash)
- **Multi-tenancy:** Per-tenant credentials, quotas, agent tokens, audit logs
- **Database:** SQLite with WAL mode, migration framework, parameterized queries (SQL injection safe)
- **Image DSL:** image.toml parser, Containerfile generator, 8-image catalog (rocky-base + 7 derived)
- **k3s scheduler:** Real pod scheduling via kube-rs, resource limits, volume mounts
- **SEV-SNP:** Attestation report generation, key sealing, measurement binding (stub on non-SEV hardware)
- **Push notifications:** Self-hosted ntfy, PQC E2E encryption, mock test harness
- **Phone PWA:** WebAuthn enrollment, sessions dashboard, REVOKE, SSE, dark mode, accessibility
- **CLI:** Full subcommand structure (tenant, credentials, agent-token, image, worker, audit, keys, init)
- **Bootstrap:** Idempotent setup scripts, systemd hardening, firewall, backup/restore, upgrade
- **CI/CD:** GitHub Actions pipeline (build, clippy, fmt, test, audit, coverage, release)
- **Testing:** 247 tests (240 unit + 7 integration), property tests, KAT vectors, 4 fuzz harnesses
- **Documentation:** Threat model, protocol spec, image DSL, operations, deployment, SEV-SNP, crypto, 10 ADRs

### Known Issues
- WebAuthn PQC gap (FIDO authenticators not deployed yet — hardware limitation, ~2027)
- SEV-SNP untested on real hardware (dev box lacks /dev/sev; software key sealing tested)

### Post-v1.0.0 Gap Fixes (all closed)
- **ML-DSA-65**: Real post-quantum signatures via ml-dsa 0.1.1 crate (was deferred)
- **PTY proxy**: Real k8s exec via kube-rs WebSocket with stdin/stdout/stderr/tty (was stub)
- **Self-signed cert**: Auto-generation via rcgen 0.14 with ECDSA P-256 (was deferred)

## [0.1.0] - 2026-07-29

### Added
- Initial scaffold of the Stronghold project.
