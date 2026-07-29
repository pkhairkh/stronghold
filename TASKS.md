# Stronghold — Implementation Tasks

> Wave-centric, subagent-based breakdown of all work required to take the Stronghold scaffold from "compiles with stubs" to "production-ready v1.0.0".
>
> **Source of truth:** This file. Every commit should reference a task ID.
> **Living document:** Update status as work progresses. Add tasks as they're discovered.

---

## Table of Contents

- [Conventions](#conventions)
- [Testing Strategy](#testing-strategy)
- [Subagent Roster](#subagent-roster)
- [Dev Environment](#dev-environment)
- [Waves](#waves)
  - [Wave 0 — Make It Compile](#wave-0--make-it-compile)
  - [Wave 1 — Crypto Foundations](#wave-1--crypto-foundations)
  - [Wave 2 — Database & Tenants](#wave-2--database--tenants)
  - [Wave 3 — Sessions & Machines](#wave-3--sessions--machines)
  - [Wave 4 — Routes & PTY Proxy](#wave-4--routes--pty-proxy)
  - [Wave 5 — Audit & Push](#wave-5--audit--push)
  - [Wave 6 — Image DSL & Builder](#wave-6--image-dsl--builder)
  - [Wave 7 — SEV-SNP Attestation](#wave-7--sev-snp-attestation)
  - [Wave 8 — Phone Enrollment & PWA](#wave-8--phone-enrollment--pwa)
  - [Wave 9 — CLI Implementation](#wave-9--cli-implementation)
  - [Wave 10 — Bootstrap & Deployment](#wave-10--bootstrap--deployment)
  - [Wave 11 — Integration & E2E](#wave-11--integration--e2e)
  - [Wave 12 — Hardening & Release](#wave-12--hardening--release)
- [Definition of Done — Project Level](#definition-of-done--project-level)

---

## Conventions

### Task IDs

```
W<wave-number>-T<task-number>
```

Example: `W0-T1` = Wave 0, Task 1.

### Task Status

Mark with one of: `[ ]` pending · `[~]` in-progress · `[x]` done · `[!]` blocked

### Commit Message Format

```
<task-id>: <imperative summary>

<body>

Refs: W<wave>-T<task>
```

Example:
```
W0-T1: fix rustls 0.23 pqc-kyber feature removal

rustls 0.23 dropped the `pqc-kyber` feature when it migrated to
aws_lc_rs as the default crypto provider. Switch to aws_lc_rs +
rustls-post-quantum crate for X25519Kyber768 hybrid key exchange.

Refs: W0-T1
```

### File Headers

Every source file touched by a task gets a header comment:
```rust
//! Stronghold Gateway — <module>
//!
//! Implemented in: W<wave>-T<task>
//! Tested by: tests/<module>_test.rs
```

---

## Testing Strategy

Stronghold is security-critical. Testing is not optional.

### Test Pyramid

| Layer | What | Tooling | Coverage Target |
|---|---|---|---|
| **Unit** | Pure functions, parsers, crypto primitives | `cargo test` | 90% line coverage |
| **Integration** | Module-to-module contracts, DB round-trips | `cargo test --features integration` | 80% of public APIs |
| **Property** | Crypto invariants, hash chains, state machines | `proptest` crate | All crypto + state code |
| **End-to-End** | Full agent → gateway → worker → pod flow | Python `pytest` + paramiko | All happy paths + key failure modes |
| **Fuzz** | Parser inputs, PTY streams, audit log tampering | `cargo fuzz` | All parsers + audit verifier |
| **Load** | Concurrent sessions, audit log throughput | `hyperfine` + custom harness | Documented baselines |
| **Security** | Static analysis, dependency audit | `cargo audit`, `cargo deny`, `clippy` | Zero warnings |

### Test Naming

```rust
#[cfg(test)]
mod tests {
    // Unit:   test_<function>_<scenario>
    // Property: proptest_<invariant>_<property>
    // Integration: it_<behavior>

    #[test]
    fn test_parse_image_toml_rejects_missing_extends() { ... }

    #[test]
    fn it_signs_and_verifies_dual_signature() { ... }

    proptest! {
        fn proptest_hash_chain_never_breaks(entries: Vec<AuditEntry>) { ... }
    }
}
```

### Test Data

- All test fixtures live under `tests/fixtures/`
- Crypto test vectors from NIST CAVP where available
- WebAuthn test assertions from `webauthn-rs` test suite
- Never use real keys in tests — generate ephemeral ones

### CI Gates (Wave 11)

A PR is mergeable only if ALL of:
- `cargo build --workspace --all-features` succeeds
- `cargo test --workspace` passes
- `cargo clippy --workspace --all-targets -- -D warnings` is clean
- `cargo fmt --all -- --check` is clean
- `cargo audit` reports no RUSTSEC advisories
- `cargo deny check` is clean
- Coverage diff is not negative

### DoD Testing Requirements

Every task's DoD MUST include:
- [ ] Unit tests for new public functions
- [ ] At least one property test for any function touching crypto or state
- [ ] At least one negative test (input that should fail)
- [ ] `cargo test <module>` passes locally on the dev box
- [ ] No new clippy warnings introduced

---

## Subagent Roster

Each task is assigned a subagent type. The orchestrating agent (you, talking to me right now) delegates by spawning subagents via the `Task` tool with the type listed below.

| Code | Subagent Type | Best For |
|---|---|---|
| **GP** | `general-purpose` | Multi-step research, doc writing, repo surgery |
| **EXP** | `Explore` | Codebase discovery, "where does X live" |
| **PLN** | `Plan` | Implementation strategy, dependency graphs |
| **FSD** | `full-stack-developer` | Anything touching HTTP routes, WebSocket, frontend HTML/JS |
| **FST** | `frontend-styling-expert` | Phone enrollment page polish, PWA UX |
| **PPT** | `ppt-expert` | Slide decks for release presentations (Wave 12) |

**Rules:**
- A subagent must read `worklog.md` before starting and append its work record after finishing.
- A subagent receives ONE task ID and the full task brief from this file.
- A subagent does NOT see conversation history — pass everything explicitly.
- Crypto, security, and audit code is NOT delegated to subagents. The orchestrating agent implements those directly.

---

## Dev Environment

The dev machine is provisioned and ready:

| Property | Value |
|---|---|
| Host | `45.63.97.103` |
| OS | Rocky Linux 10.2 (Red Quartz) |
| Kernel | 6.12.0-211.34.1.el10_2.x86_64 |
| CPU | 8 × AMD EPYC-Turin |
| RAM | 31 GB |
| Disk | 473 GB (442 GB free) |
| `/dev/sev` | **Not present** (develop with `--features no-sev-snp`) |
| `/dev/kvm` | Present (Firecracker viable if needed later) |
| Rust | 1.97.1 (stable, via rustup) |
| Repo | `/root/stronghold` (cloned from `github.com/pkhairkh/stronghold`) |
| Build | `cd /root/stronghold && cargo build --workspace --features no-sev-snp` |

**SSH access** (orchestrating agent only):
```bash
python3 /home/z/my-project/scripts/ssh_exec.py '<command>'
python3 /home/z/my-project/scripts/ssh_exec.py --file <local_script.sh>
python3 /home/z/my-project/scripts/ssh_exec.py --upload <local> <remote>
```

**Scaffold state:** 19 compile errors (catalogued in Wave 0). All functions are `todo!()` stubs. Architecture, docs, and ADRs are complete.

---

## Waves

### Wave 0 — Make It Compile

**Goal:** `cargo build --workspace --features no-sev-snp` succeeds with zero errors and zero warnings on the dev box. No new functionality — just fix the scaffold.

**Subagent:** Orchestrating agent (no delegation — too cross-cutting).

**Current state (as of scaffold commit `f64f75f`):** 19 errors, 9 warnings.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W0-T1 | Fix `rustls` 0.23 `pqc-kyber` feature removal | `Cargo.toml`, `gateway/Cargo.toml` | `cargo build` resolves `rustls`. PQ TLS configured via `aws_lc_rs` provider + `rustls-post-quantum` crate. | `cargo build --workspace` succeeds past rustls resolution. |
| W0-T2 | Fix `audit` module file-not-found | `gateway/src/audit/mod.rs` | `mod audit` resolves. Either rename `log.rs`/`verify.rs`/`export.rs` to be properly declared, or fix `mod.rs` to point at them. | `cargo check` no longer reports E0583 for `audit`. |
| W0-T3 | Fix `OrderResponse` unresolved import in `sessions/manager.rs` | `gateway/src/sessions/manager.rs` | Remove duplicate `use crate::routes::OrderResponse as SessResponse`. Single clean import. | `cargo check` no longer reports E0432. |
| W0-T4 | Add `async_stream` dependency | `Cargo.toml`, `gateway/Cargo.toml` | Add `async-stream = "0.3"` to workspace deps. Use it in `pending_approval_stream`. | E0433 resolved. |
| W0-T5 | Fix lifetime in `sessions/scopes.rs::load` | `gateway/src/sessions/scopes.rs` | `fn load<'a>(path: &'a str) -> Result<ScopeConfig>` — return owned, not borrowed. | Compiles without E0106. |
| W0-T6 | Fix `attestation::get_report` return type | `gateway/src/routes/attestation.rs` | Return `Result<Json<AttestationResponse>, (StatusCode, String)>`. Match axum 0.7 handler signature. | E0308 resolved. |
| W0-T7 | Fix type annotations in `sessions/manager.rs::wait_for_decision` | `gateway/src/sessions/manager.rs` | Annotate the closure's return type explicitly. Replace `loop { ... }` with `tokio::time::interval` if cleaner. | E0282/E0283 resolved. |
| W0-T8 | Remove `Debug` derive from `PushKeys` (contains `StaticSecret`) | `gateway/src/crypto/hybrid_kem.rs` | `PushKeys` no longer derives `Debug` (or implements it manually without exposing secret bytes). | E0277 resolved. |
| W0-T9 | Import `base64::Engine` trait everywhere it's used | `gateway/src/crypto/hybrid_kem.rs`, `hybrid_sig.rs`, `push/e2e.rs`, `tenants/auth.rs` | Add `use base64::Engine;` at top of each file using `.encode()`/`.decode()` on `GeneralPurpose`. | E0599 resolved for all base64 call sites. |
| W0-T10 | Fix `[u8]` size errors in `hybrid_sig.rs` | `gateway/src/crypto/hybrid_sig.rs` | Use `Vec<u8>` or fixed-size arrays (`[u8; 32]`, `[u8; 1952]`) where appropriate. `ed25519_dalek::Signature::from_slice` returns `Result`, handle it. | E0277 resolved. |
| W0-T11 | Fix `aes_gcm::Error` not implementing `StdError` | `gateway/src/push/e2e.rs` | Map `aes_gcm::Error` to `anyhow::Error` via `.map_err(\|e\| anyhow::anyhow!("aes-gcm: {:?}", e))?`. | E0277 resolved. |
| W0-T12 | Silence unused-variable warnings | All files listed in warnings | Prefix unused params with `_` or remove. | `cargo build` is warning-free. |
| W0-T13 | Verify `cargo clippy --workspace --all-features -- -D warnings` is clean | workspace | Fix any clippy lints. | Clippy exits 0. |
| W0-T14 | Verify `cargo test --workspace` runs (no tests yet, but collection works) | workspace | `cargo test` collects and runs 0 tests without compile errors. | `cargo test` exits 0. |
| W0-T15 | Tag `v0.1.1-scaffold-compiles` | repo | Git tag pushed. CI (if any) green. | Tag exists on `main`. |

**Wave 0 DoD:**
- [ ] `cargo build --workspace --features no-sev-snp` exits 0 on dev box
- [ ] `cargo build --workspace --features sev-snp` exits 0 on dev box (SEV code can be dead-stripped)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo test --workspace` collects without errors
- [ ] Commit pushed, tag `v0.1.1-scaffold-compiles` created

---

### Wave 1 — Crypto Foundations

**Goal:** Real implementations of all cryptographic primitives. No stubs. Every function tested with known-answer tests (KATs) where vectors exist.

**Subagent:** Orchestrating agent only (no delegation — security-critical).

**Why first:** Every other module depends on crypto. Audit log signing, push encryption, TLS config, WebAuthn verification — all need working crypto before integration tests can run.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W1-T1 | Real `AuditKeys` keypair generation + save/load to disk | `gateway/src/crypto/hybrid_sig.rs` | Keypair persisted as PEM or raw bytes in `/var/lib/stronghold/keys/`. Loaded on startup. Files mode 0600. | Unit: generate → save → load → sign → verify round-trip. Property: 1000 random messages, sign+verify all pass. KAT: Ed25519 RFC 8032 test vectors. |
| W1-T2 | Real `DualSignature` sign + verify (Ed25519 only initially) | `gateway/src/crypto/hybrid_sig.rs` | `sign(message)` returns `DualSignature` with valid Ed25519 sig. `verify(message, sig)` returns true for valid, false for tampered. | Unit: sign/verify round-trip. Negative: bit-flip in message → verify false. Negative: bit-flip in signature → verify false. |
| W1-T3 | Real ML-DSA-65 signing (when `ml-dsa` crate is stable) | `gateway/src/crypto/hybrid_sig.rs` | Both Ed25519 and ML-DSA-65 signatures populated. `verify` checks both. | KAT: NIST FIPS 204 ML-DSA-65 test vectors. Property: 1000 random messages, dual-sign + dual-verify all pass. |
| W1-T4 | Real `PushKeys` (X25519 + ML-KEM-768) keypair | `gateway/src/crypto/hybrid_kem.rs` | Both keypairs generated, saved, loaded. Public halves serializable to JSON for phone enrollment. | Unit: generate → save → load → encapsulate → decapsulate round-trip. KAT: X25519 RFC 7748 test vectors. |
| W1-T5 | Real `encapsulate` / `decapsulate` (hybrid KEM) | `gateway/src/crypto/hybrid_kem.rs` + `push/e2e.rs` | `encapsulate(phone_pub) → (EncapsulatedSecret, shared_secret)`. Phone-side `decapsulate` (in WASM) recovers same `shared_secret`. HKDF derivation deterministic. | Unit: encapsulate → derive AES key → encrypt → decrypt round-trip. Property: 1000 encapsulations, all produce different shared secrets. Negative: wrong phone pubkey → decapsulate fails. |
| W1-T6 | Real `derive_aes_key` via HKDF-256 | `gateway/src/crypto/hybrid_kem.rs` | HKDF with SHA-256, info string `"stronghold-push-v1"` or `"stronghold-audit-v1"`. | KAT: RFC 5869 HKDF test vectors. |
| W1-T7 | TLS server config with X25519Kyber768 hybrid | `gateway/src/crypto/tls.rs` | `build_server_config()` returns a `rustls::ServerConfig` with PQ hybrid key exchange enabled. Self-signed cert generated at install time. | Integration: spin up server, connect with `openssl s_client -curves X25519Kyber768Draft00`, verify handshake. Unit: assert cipher suite in negotiated params. |
| W1-T8 | WebAuthn assertion verification (real, not stub) | `gateway/src/crypto/webauthn.rs` | `verify_assertion` checks signature, challenge, origin, user-verified flag. Rejects tampered challenges. | Integration: use `webauthn-rs` test fixtures. Negative: wrong challenge → false. Negative: wrong origin → false. Negative: `user_verified=false` → false. |
| W1-T9 | WebAuthn challenge generation bound to session | `gateway/src/crypto/webauthn.rs` | `generate_challenge(cmd_hash, request_id, scope_hash)` returns 32 bytes. Documented binding: phone signs `(session_id, scope_hash, ttl, sev_snp_measurement)`. | Unit: same inputs → same challenge. Property: different `request_id` → different challenge. |
| W1-T10 | Crypto test fixtures | `tests/fixtures/crypto/` | NIST CAVP vectors for ML-KEM-768, ML-DSA-65, X25519, Ed25519, HKDF-SHA256, AES-256-GCM. Committed to repo. | All KAT tests pass against these fixtures. |
| W1-T11 | Crypto fuzzing harnesses | `fuzz/` (new dir) | `cargo fuzz` targets for: `parse_image_toml`, `verify_audit_chain`, `webauthn_assertion_decode`. Run for 1M iterations each on CI. | No panics in 1M iterations. Documented crash corpus if any. |

**Wave 1 DoD:**
- [ ] All crypto functions have real implementations (no `todo!()` in `crypto/`)
- [ ] 90%+ line coverage in `crypto/` modules
- [ ] All NIST KAT vectors pass
- [ ] `cargo fuzz run --release` for 1M iterations on each target, no panics
- [ ] `cargo audit` clean
- [ ] Documented in `docs/CRYPTO.md` (new file) with algorithm choices, key sizes, and rationale

---

### Wave 2 — Database & Tenants

**Goal:** SQLite pool works, schema is migrated cleanly, tenants can be created/listed/quoted/authenticated.

**Subagent:** Orchestrating agent for schema + migrations. `full-stack-developer` for tenant registry CRUD endpoints.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W2-T1 | Real `init_pool` with schema.sql execution | `gateway/src/db/mod.rs` | Pool created, schema applied, indexes built. Idempotent (safe to re-run). | Unit: init fresh DB → all tables exist → re-init → no errors. |
| W2-T2 | Database migrations framework | `gateway/src/db/migrations.rs` | Numbered SQL files under `gateway/migrations/`. Applied in order. Recorded in `_migrations` table. | Unit: migrate fresh → v1. Apply v2 → v2 recorded. Skip already-applied. |
| W2-T3 | Tenant registry: real `create` / `get` / `list` | `gateway/src/tenants/registry.rs` | Tenants persist across restarts. `setup_password` stored hashed (SHA-256). `setup_used` flag prevents reuse. | Unit: create → get → list → assert fields. Negative: get non-existent → error. Property: 100 tenants created, list returns all 100. |
| W2-T4 | Tenant quotas: real `set` / `get` / `check_capacity` | `gateway/src/tenants/quotas.rs` | Quotas enforced. `check_capacity` correctly counts active machines. | Unit: set quota → check passes → add machines until limit → check fails. Property: quota enforcement is correct for random concurrent machine counts. |
| W2-T5 | Agent token minting and verification | `gateway/src/tenants/auth.rs` | Tokens are 32 random bytes, base64url-encoded, prefixed `stronghold_agent_`. Stored as SHA-256 hash. TTL enforced. | Unit: mint → verify → assert tenant_id matches. Negative: expired token → 401. Negative: revoked token → 401. Negative: tampered token → 401. |
| W2-T6 | Phone token issuance and verification | `gateway/src/tenants/auth.rs` | Phone tokens issued at credential enrollment. Long-lived, revocable. Stored as SHA-256 hash. | Unit: issue → verify → revoke → verify fails. |
| W2-T7 | WebAuthn credential enrollment (server side) | `gateway/src/tenants/auth.rs` | `enroll_credential` stores credential with public key, aaguid, transports. Marks `setup_used=1`. Issues phone token. | Integration: enroll via `POST /phone/enroll` with real WebAuthn response → 200, credential in DB, phone token returned. |
| W2-T8 | SQL injection hardening audit | all `rusqlite` call sites | Every query uses parameterized `params![]`. No string concatenation for SQL. | Manual review + `grep` for `format!` near `execute`/`query`. Property: fuzz SQL inputs, no injection possible. |
| W2-T9 | Database backup/restore | `gateway/src/db/backup.rs` (new) | `backup_to(path)` uses SQLite online backup API. `restore_from(path)` swaps the DB. | Unit: backup → modify DB → restore → assert original state. |
| W2-T10 | Per-tenant audit log databases | `gateway/src/audit/log.rs` | Each tenant gets `/var/lib/stronghold/audit/<tenant_id>.db`. Audit entries go to tenant-specific DB. | Unit: write entries for tenant A and B → assert A's DB has only A's entries. |

**Wave 2 DoD:**
- [ ] `cargo test --workspace --features integration` passes all DB tests
- [ ] SQLite WAL mode enabled for concurrent reads
- [ ] All queries parameterized (no SQL injection vectors)
- [ ] 90%+ line coverage in `db/` and `tenants/`
- [ ] Backup/restore tested end-to-end

---

### Wave 3 — Sessions & Machines

**Goal:** Session lifecycle works end-to-end. Pods can be scheduled on k3s workers. VPS escalation functional.

**Subagent:** Orchestrating agent for session manager (state machine, security-critical). `full-stack-developer` for k3s scheduler integration.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W3-T1 | Real `create_pending` session | `gateway/src/sessions/manager.rs` | Pending session inserted with status `pending`. TTL recorded. Image recorded. | Unit: create → query → assert fields match. |
| W3-T2 | Real `wait_for_decision` with SSE polling | `gateway/src/sessions/manager.rs` | Long-polls DB for status change. Returns `Approved`/`Denied`/`Timeout`. No busy-wait (uses `tokio::time::interval` with 500ms tick). | Integration: create pending → approve in another task → `wait_for_decision` returns `Approved`. Timeout test: 60s with no decision → `Timeout`. |
| W3-T3 | Real `approve_session` / `deny_session` | `gateway/src/sessions/manager.rs` | Updates `pending_sessions.status` to `approved` or `denied`. Records `decided_at`. | Unit: approve → query → status is `approved`. |
| W3-T4 | Real `finalize_session` (schedule pod + return connect token) | `gateway/src/sessions/manager.rs` + `machines/scheduler.rs` | Calls scheduler, inserts into `machines` table, generates connect token, writes audit entry. | Integration: approve session → assert pod scheduled on worker, machine row exists, audit entry written. |
| W3-T5 | Real `resume_session` | `gateway/src/sessions/manager.rs` | Validates machine exists, belongs to tenant, not expired. Returns fresh connect token. | Unit: resume active → ok. Resume expired → `SessionError::Expired`. Resume non-existent → `SessionError::NotFound`. |
| W3-T6 | Real `release_session` / `revoke_session` | `gateway/src/sessions/manager.rs` | Kills pod via scheduler. Updates `machines.status`. Writes audit entry. Snapshot volumes. | Integration: release → pod killed, status `released`, audit entry written. Revoke → same but status `revoked`. |
| W3-T7 | Real `create_extend_request` + `finalize_extend` | `gateway/src/sessions/manager.rs` | Extend creates new pending session with `is_extend=1`. On approval, updates machine's `expires_at`. | Integration: extend → approve → assert TTL extended in DB. |
| W3-T8 | Quorum matching for destructive ops | `gateway/src/sessions/scopes.rs` + `manager.rs` | When agent runs destructive command mid-session, command blocks. Pushes all tenant credentials. Requires N approvals. | Integration: agent runs `rm -rf` → push sent → 1 approval → blocks → 2nd approval → executes. Negative: 1 approval + 60s timeout → command denied. |
| W3-T9 | k3s scheduler: real pod creation | `gateway/src/machines/scheduler.rs` | Uses `kube-rs` (add to deps) to call k3s API. Creates Pod with image, resource limits, volume mounts. | Integration: schedule pod on real k3s → `kubectl get pods` shows it. |
| W3-T10 | k3s scheduler: real pod deletion | `gateway/src/machines/scheduler.rs` | `kill_pod(machine_id)` deletes the Pod. Graceful shutdown with 30s grace period. | Integration: kill pod → `kubectl get pods` shows terminating → gone after 30s. |
| W3-T11 | PTY handle: real containerd exec | `gateway/src/machines/scheduler.rs` | `open_pty(machine_id)` opens WebSocket to k3s exec API. Bidirectional bytes. | Integration: open PTY → send `echo hello` → receive `hello`. |
| W3-T12 | Worker registration and capacity tracking | `gateway/src/machines/worker.rs` | Workers register via k3s node API. Capacity (CPU, memory, disk) queried. `find_worker` picks best fit. | Unit: register worker → list shows it. Capacity: 2 workers, pick the one with more free RAM. |
| W3-T13 | Vultr VPS escalation | `gateway/src/machines/escalation.rs` | Calls Vultr API (`POST /v2/instances`). Cloud-init script joins k3s. On session end, calls `DELETE`. Snapshot volumes to object storage. | Integration: escalate → VPS boots → joins k3s → pod scheduled. End session → VPS destroyed. (Use Vultr sandbox account.) |
| W3-T14 | cgroup v2 resource limits per pod | k8s Pod spec | Pod spec includes `resources.limits` for CPU and memory. Enforced by k3s. | Integration: schedule pod with 2 CPU / 4GB → `kubectl describe pod` shows limits → stress test inside pod caps at limits. |
| W3-T15 | Network policy: default-deny egress | Calico/Cilium NetworkPolicy | Per-tenant NetworkPolicy. Default deny. Allowlist (github.com, crates.io, etc.) applied. | Integration: pod cannot `curl evil.com` → connection refused. Pod can `curl github.com` → 200. |

**Wave 3 DoD:**
- [ ] Full session lifecycle works: ORDER → approve → PTY → RELEASE
- [ ] RESUME works across gateway restarts
- [ ] Quorum blocks destructive ops until N approvals
- [ ] Pods scheduled on real k3s worker
- [ ] VPS escalation boots and destroys a real Vultr instance
- [ ] Network policy enforced
- [ ] 80%+ line coverage in `sessions/` and `machines/`

---

### Wave 4 — Routes & PTY Proxy

**Goal:** All HTTP/WebSocket endpoints functional. PTY proxy streams bytes bidirectionally. Anomaly scanner runs on PTY stream.

**Subagent:** `full-stack-developer` for route handlers. Orchestrating agent for PTY proxy (security-critical byte stream).

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W4-T1 | `/agent/order` real handler | `gateway/src/routes/agent.rs` | Validates token, creates pending session, pushes ntfy, long-polls for decision, finalizes on approval. Returns 200/403/408. | Integration: curl ORDER → ntfy push received → approve → 200 with machine_id. Timeout → 408. Deny → 403. |
| W4-T2 | `/agent/resume`, `/agent/release`, `/agent/extend` real handlers | `gateway/src/routes/agent.rs` | All three work end-to-end. Proper error codes (404, 410, 403, 408). | Integration: full lifecycle test. |
| W4-T3 | `/agent/health` real handler | `gateway/src/routes/agent.rs` | Returns 200 if gateway up + DB reachable + SEV-SNP attested (or `--dev`). 503 otherwise. | Unit: healthy → 200. DB down → 503. |
| W4-T4 | WebSocket PTY proxy | `gateway/src/routes/pty.rs` | `handle_pty_ws` upgrades to WebSocket, verifies connect token, opens containerd exec, proxies bytes bidirectionally. | Integration: open WS → send bytes → receive bytes. Close WS → exec terminates. |
| W4-T5 | WebSocket audit stream (read-only) | `gateway/src/routes/pty.rs` | `handle_audit_ws` streams audit events for a machine_id to the phone browser (for "WATCH LIVE"). | Integration: open WS → audit events flow as session runs. |
| W4-T6 | `/phone/pending` SSE stream | `gateway/src/routes/phone.rs` | SSE connection authenticated via phone token. Pushes `approval_request` events. Heartbeat every 30s. Reconnect on drop. | Integration: connect SSE → trigger ORDER → event received within 1s. |
| W4-T7 | `/phone/decide` real handler | `gateway/src/routes/phone.rs` | Verifies WebAuthn assertion. Calls `approve_session` or `deny_session`. Returns 200/401. | Integration: post decision → session status changes. |
| W4-T8 | `/phone/revoke` real handler | `gateway/src/routes/phone.rs` | Calls `revoke_session`. Returns 200. Phone UI updates. | Integration: post revoke → session killed within 500ms. |
| W4-T9 | `/phone/enroll` real handler | `gateway/src/routes/phone.rs` | Verifies setup_password. Stores WebAuthn credential. Issues phone token. Returns 200. | Integration: enroll via real WebAuthn flow → credential in DB. |
| W4-T10 | `/setup` serves enrollment page | `gateway/src/routes/phone.rs` | Returns `phone/enroll.html`. Sets `Content-Type: text/html`. | Unit: GET /setup → 200, body contains `<form>`. |
| W4-T11 | `/attestation` real handler | `gateway/src/routes/attestation.rs` | Returns SEV-SNP report (or stub if `--dev`). Signed by gateway key. | Unit: GET /attestation → 200, JSON has `measurement` field. |
| W4-T12 | `/admin/tenant` create + get | `gateway/src/routes/admin.rs` | Admin-authenticated. Creates tenant, returns setup_password + enrollment URL. | Integration: POST /admin/tenant → 201 with setup_password. |
| W4-T13 | Anomaly scanner integration into PTY stream | `gateway/src/anomaly/mod.rs` + `routes/pty.rs` | Scanner runs on every PTY byte. Matches → push ntfy (non-blocking). All matches logged. | Integration: run `curl evil.com` in PTY → anomaly push sent within 1s. |
| W4-T14 | Rate limiting on `/agent/*` | `tower-http::limit` | Per-agent-token rate limit: 10 ORDERs/minute. Prevents abuse. | Integration: 11th ORDER in a minute → 429. |
| W4-T15 | Request tracing + structured logging | `gateway/src/main.rs` + all routes | Every request logged with `tracing` span: tenant_id, machine_id, method, path, status, latency. | Integration: make requests → logs contain all fields in JSON. |

**Wave 4 DoD:**
- [ ] All routes in `routes/mod.rs` have real handlers (no `todo!()`)
- [ ] PTY proxy streams bytes without corruption (verified by hex diff)
- [ ] Anomaly scanner detects all patterns in `anomaly.toml`
- [ ] 80%+ line coverage in `routes/`
- [ ] Load test: 100 concurrent PTY sessions, no errors

---

### Wave 5 — Audit & Push

**Goal:** Audit log is dual-signed, hash-chained, verifiable offline. Push notifications are E2E-encrypted and delivered.

**Subagent:** Orchestrating agent for audit (security-critical). `full-stack-developer` for ntfy integration.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W5-T1 | Real audit log entry writer | `gateway/src/audit/log.rs` | `entry()` computes prev_hash, signs message with both Ed25519 + ML-DSA-65, computes hash, inserts. Atomic transaction. | Unit: write 100 entries → hash chain intact → all signatures verify. |
| W5-T2 | Real audit log verifier | `gateway/src/audit/verify.rs` | `verify_tenant(tenant_id)` walks chain, verifies every signature, flags breaks. Returns detailed report. | Unit: verify clean log → OK. Tamper one entry → verify fails with specific error. Property: 1000 random logs, tamper random entry, verify always catches it. |
| W5-T3 | Audit log exporter | `gateway/src/audit/export.rs` | Export to JSON or text. Filter by date range, machine_id. Streaming for large logs. | Unit: export → parse JSON → assert count matches. |
| W5-T4 | Key rotation ceremony | `gateway/src/audit/log.rs` + CLI | `rotate_audit_keys`: generate new keypair, sign `key_rotation` entry with old keys, all subsequent entries use new keys. Old keys retained for verification. | Integration: rotate → entries before rotation verify with old keys, after with new keys. Both still verify in `audit verify`. |
| W5-T5 | ntfy client: real HTTP push | `gateway/src/push/ntfy.rs` | POSTs to local ntfy server. Supports action buttons (`view, Approve, URL`). Sets priority. Handles 429 rate limit. | Integration: push → ntfy server receives → phone app shows notification with buttons. |
| W5-T6 | E2E push encryption | `gateway/src/push/e2e.rs` | Encrypts payload with phone's X25519 + ML-KEM-768 public keys. Encodes as base64 JSON. ntfy sees ciphertext only. | Unit: encrypt → decrypt (with phone private keys) → matches plaintext. Negative: wrong phone keys → decrypt fails. |
| W5-T7 | ntfy server setup + ACLs | `setup/ntfy.yml` (new) | ntfy configured with per-tenant topics. Auth required. Attachments disabled (we send inline). | Integration: subscribe without auth → 401. With auth → receives pushes. |
| W5-T8 | Phone-side PQC WASM bundle | `phone/pq-wasm/` | `@noble/post-quantum` bundled to ~12KB gzipped. Exposes `generateKeyPairs()`, `decapsulate(encapsulated)`. Loaded by `enroll.html`. | Unit: WASM functions work in headless browser (Playwright). Round-trip: gateway encapsulates → WASM decapsulates → shared secret matches. |
| W5-T9 | Daily audit digest push | `gateway/src/push/ntfy.rs` | Cron-like task: at 09:00 tenant-local, push summary: # sessions, # commands, # anomalies. | Integration: trigger digest → push sent with correct counts. |
| W5-T10 | Audit log tamper detection fuzzing | `fuzz/audit_verify` | Fuzz the verifier with corrupted logs. Must never crash, always return correct verdict. | 1M iterations, no panics, no false accepts. |

**Wave 5 DoD:**
- [ ] Audit log signs every entry with both algorithms
- [ ] Verifier catches any single-bit tamper
- [ ] Push notifications arrive on phone within 2s of trigger
- [ ] E2E encryption: ntfy server cannot read content (verified by tcpdump)
- [ ] 90%+ line coverage in `audit/` and `push/`

---

### Wave 6 — Image DSL & Builder

**Goal:** `image.toml` files parse correctly. Containerfiles generate. Images build via podman and push to ghcr.io.

**Subagent:** `full-stack-developer` for DSL + builder. Orchestrating agent for rocky-base image (security-critical base).

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W6-T1 | Real `image.toml` parser | `gateway/src/images/dsl.rs` | Parses all 8 catalog images. Validates `extends` chain back to `rocky-base`. Clear errors for missing fields. | Unit: parse each catalog image → OK. Property: fuzz parser with random TOML, never panics. Negative: missing `name` → error. |
| W6-T2 | Real Containerfile generator | `gateway/src/images/builder.rs` | `generate_containerfile(config)` produces valid Containerfile. Includes `FROM`, `RUN`, `ENV`, `LABEL`, escape-hatch snippets. | Unit: generate → diff against golden files in `tests/fixtures/containerfiles/`. |
| W6-T3 | Real image builder (calls podman) | `gateway/src/images/builder.rs` | `build(config, tag)` writes Containerfile to temp dir, runs `podman build`, returns image digest. | Integration: build `rocky-base` → image exists in local podman store. |
| W6-T4 | OCI registry push/pull | `gateway/src/images/registry.rs` | Uses `oci-distribution` crate. Push to ghcr.io (public) or local registry (private). Pull with auth. | Integration: push to local registry → pull from another box → image matches by digest. |
| W6-T5 | `rocky-base` image builds and works | `images/rocky-base/` + CI | Image builds via `podman build`. Contains all packages in `image.toml`. `dev` user exists with sudo. Fish shell default. | Integration: `podman run -it stronghold/rocky-base fish -c 'echo hello'` → `hello`. `whoami` → `dev`. `sudo whoami` → `root`. |
| W6-T6 | All 7 derived images build | `images/*/` | Each builds FROM rocky-base. Toolchain works: `rustc --version` in rust-nightly, `node --version` in node-20, etc. | Integration: run each image, assert toolchain version matches pin. |
| W6-T7 | Image vulnerability scanning in CI | `.github/workflows/images.yml` (new) | Trivy scans every image. Fail CI on CRITICAL or HIGH vulnerabilities. | CI scans rocky-base → 0 CRITICAL. |
| W6-T8 | Image DSL escape hatches work | `gateway/src/images/builder.rs` | `pre_install`, `post_install`, `inject_containerfile` all produce correct Containerfile output. | Unit: image with all three escape hatches → generated Containerfile contains snippets in right places. |
| W6-T9 | Private tenant images | `gateway/src/images/registry.rs` | Tenants can build and push private images to local registry. ACL'd per-tenant. | Integration: tenant A builds private image → tenant B cannot pull → 403. |
| W6-T10 | Image catalog CI: build all on PR | `.github/workflows/images.yml` | PR touching `images/` triggers CI build of all images. Push to ghcr.io on merge to main. | CI green on PR. Images appear on ghcr.io after merge. |

**Wave 6 DoD:**
- [ ] All 8 catalog images build and push to ghcr.io
- [ ] `stronghold image build` CLI command works
- [ ] Trivy scans clean (no CRITICAL/HIGH)
- [ ] 90%+ line coverage in `images/`

---

### Wave 7 — SEV-SNP Attestation

**Goal:** Gateway runs inside SEV-SNP guest. Attestation report verifiable by phone. Keys sealed to measurement.

**Subagent:** Orchestrating agent only (security-critical, requires SEV-SNP hardware).

**Note:** Dev box lacks `/dev/sev`. Tests run on a SEV-SNP Vultr plan (provisioned in W7-T1). Unit tests mock the SEV driver.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W7-T1 | Provision SEV-SNP Vultr box for testing | infra | New Vultr HF plan with SEV-SNP enabled. Rocky 10. `/dev/sev` present. | Box reachable, `/dev/sev` exists, `grep sev /proc/cpuinfo` shows `sev sev_es`. |
| W7-T2 | Real SEV-SNP attestation report generation | `gateway/src/tee/sev_snp.rs` | Uses `sev` crate to call `/dev/sev` ioctl. Returns signed attestation report with measurement. | Integration on SEV box: `stronghold-gateway attestation` → valid report. |
| W7-T3 | Key sealing to measurement | `gateway/src/tee/sev_snp.rs` | `seal_keys(keys)` encrypts with key derived from launch measurement. `unseal_keys(sealed)` recovers only on same measurement. | Unit: seal → unseal on same box → OK. Modify binary → unseal fails (tested by changing a byte and rebuilding). |
| W7-T4 | Phone verifies attestation before enrollment | `phone/enroll.html` | `GET /attestation` on page load. Display measurement prominently. Compare with `docs/MEASUREMENTS/v1.0.txt`. Refuse to enroll if mismatch. | Integration: enroll on matching measurement → OK. Tamper with binary → mismatch → enrollment refused. |
| W7-T5 | Attestation binding in WebAuthn challenge | `gateway/src/crypto/webauthn.rs` | Challenge includes `sev_snp_measurement` hash. WebAuthn assertion signs it. Subsequent approvals verify measurement hasn't changed. | Unit: same measurement → verify OK. Different measurement → verify fails. |
| W7-T6 | Key rotation ceremony on upgrade | `gateway/src/audit/log.rs` + `tee/sev_snp.rs` | `stronghold upgrade` re-attests with new measurement. Old keys unsealed with old measurement, new keys sealed to new. Audit entry signed by both. | Integration: upgrade gateway → audit log still verifies, new entries signed with new keys. |
| W7-T7 | `--features no-sev-snp` stub correctness | `gateway/src/tee/no_sev.rs` | Stub returns `sev_snp_active: false`. Audit entries lack `sev_snp_report_hash`. `audit verify` warns but doesn't fail. | Unit: stub functions return documented values. |
| W7-T8 | Measurement registry | `docs/MEASUREMENTS/` | Each release tags a measurement file. GPG-signed. Published in release notes. | W7-T8 measurement file exists, GPG signature verifies. |
| W7-T9 | SEV-SNP integration test suite | `tests/sev_snp/` | Full attestation flow on real SEV box. Captured as golden test. | Tests pass on SEV box. Skipped on non-SEV box. |

**Wave 7 DoD:**
- [ ] Gateway boots inside SEV-SNP guest on real Vultr SEV box
- [ ] Attestation report verifiable by phone
- [ ] Keys sealed to measurement (binary modification breaks unseal)
- [ ] Audit log entries include `sev_snp_report_hash` when running in TEE
- [ ] `--features no-sev-snp` build works on dev box without SEV

---

### Wave 8 — Phone Enrollment & PWA

**Goal:** Phone enrollment page works in mobile Safari/Chrome. WebAuthn ceremonies functional. PWA installable.

**Subagent:** `frontend-styling-expert` for UI polish. `full-stack-developer` for WebAuthn + WASM integration.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W8-T1 | Real WebAuthn enrollment flow | `phone/enroll.html` | `navigator.credentials.create()` with platform authenticator. Sends credential to gateway. Handles errors. | Integration: enroll on iPhone Safari → Face ID prompt → credential stored. Same on Android Chrome. |
| W8-T2 | Real WebAuthn approval flow | `phone/enroll.html` | `navigator.credentials.get()` with `userVerification: 'required'`. Sends assertion to `/phone/decide`. | Integration: tap Approve → Face ID → assertion posted → session approved. |
| W8-T3 | PQC WASM bundle for push decryption | `phone/pq-wasm/` | `@noble/post-quantum` bundled. Exposes `generateKeyPairs()`, `decapsulate()`. Keys stored in IndexedDB (non-extractable). | Unit: WASM loads in browser. Key generation works. Round-trip with gateway. |
| W8-T4 | Active sessions dashboard | `phone/enroll.html` | Shows all active sessions for tenant: image, TTL remaining, CPU/mem usage, last command. REVOKE button per session. | Integration: ORDER a session → appears in dashboard within 2s. Tap REVOKE → session killed. |
| W8-T5 | Pending approvals list | `phone/enroll.html` | Shows pending ORDERs. Approve/Deny buttons. Auto-refreshes via SSE. | Integration: agent ORDERs → appears in list → approve → disappears. |
| W8-T6 | PWA manifest + service worker | `phone/manifest.json`, `phone/sw.js` | Installable to home screen. Works offline (cached shell). Splash screen. | Integration: "Add to Home Screen" → icon appears → launches fullscreen. |
| W8-T7 | Quorum approval UI | `phone/enroll.html` | When destructive op needs quorum, shows "N of M approvals required". Updates as approvals come in. | Integration: trigger destructive op → UI shows "1 of 2" → second phone approves → "2 of 2" → executes. |
| W8-T8 | Mobile UX polish | `phone/enroll.html` + CSS | Large tap targets (min 44pt). Dark mode. Haptic feedback on Approve/Deny. Accessible (VoiceOver compatible). | Manual review on iPhone + Android. Lighthouse score >90. |
| W8-T9 | Anomaly alert UI | `phone/enroll.html` | Anomaly pushes deep-link to detail page showing the suspicious command + context. REVOKE button. | Integration: trigger anomaly → notification opens detail page → tap REVOKE → session killed. |
| W8-T10 | Cross-browser testing matrix | `tests/e2e/browser/` | Safari iOS 17+, Chrome Android latest, Firefox Android latest, Safari macOS, Chrome macOS. WebAuthn + WASM work on all. | Playwright tests pass on all 5 browsers. |

**Wave 8 DoD:**
- [ ] Enrollment works on iPhone Safari + Android Chrome
- [ ] PWA installable, launches fullscreen
- [ ] Active sessions dashboard real-time
- [ ] Approve/Deny/Revoke all functional
- [ ] PQC WASM bundle <15KB gzipped
- [ ] Lighthouse score >90 on mobile

---

### Wave 9 — CLI Implementation

**Goal:** `stronghold` CLI talks to gateway API. All subcommands functional.

**Subagent:** `full-stack-developer` for CLI commands.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W9-T1 | `stronghold tenant create/list/get` | `cli/src/main.rs` | Calls `/admin/tenant`. Prints setup_password + enrollment URL. | Integration: create tenant → list shows it → get returns details. |
| W9-T2 | `stronghold credentials enroll/list/revoke` | `cli/src/main.rs` | `enroll` prints URL to open. `list` shows table. `revoke` calls API. | Integration: enroll → list shows credential → revoke → list shows revoked. |
| W9-T3 | `stronghold agent-token mint/list/revoke` | `cli/src/main.rs` | Mints token, prints once. `list` shows active tokens. `revoke` invalidates. | Integration: mint → use token for ORDER → revoke → ORDER fails with 401. |
| W9-T4 | `stronghold image build/list/push` | `cli/src/main.rs` | Builds from image.toml. Lists catalog. Pushes to registry. | Integration: build rocky-base → list shows it → push to local registry. |
| W9-T5 | `stronghold worker add/list` | `cli/src/main.rs` | SSHes to worker, runs bootstrap. Lists registered workers. | Integration: add worker → list shows it → k3s node ready. |
| W9-T6 | `stronghold audit verify/export` | `cli/src/main.rs` | `verify` walks chain, prints report. `export` writes JSON/text to stdout or file. | Integration: verify clean log → OK. Export → valid JSON. |
| W9-T7 | `stronghold keys rotate-audit/rotate-push` | `cli/src/main.rs` | Calls gateway to rotate keys. Prints confirmation. | Integration: rotate → audit log shows `key_rotation` entry → verify still passes. |
| W9-T8 | `stronghold init` | `cli/src/main.rs` | Initializes data dir, DB, keys. Prints setup instructions. | Integration: fresh box → `init` → all dirs created, DB initialized, keys generated. |
| W9-T9 | CLI config file (`~/.stronghold.toml`) | `cli/src/main.rs` | Reads gateway URL + admin token from config. `--url` and `--admin-token` flags override. | Unit: config file parsed. Flag overrides config. |
| W9-T10 | CLI shell completion | `cli/src/main.rs` | `stronghold completions --shell bash/zsh/fish` generates completion script. | Integration: source completion → tab completion works. |

**Wave 9 DoD:**
- [ ] All CLI subcommands functional
- [ ] CLI works against real gateway (integration tested)
- [ ] Shell completion for bash, zsh, fish
- [ ] 80%+ line coverage in `cli/`

---

### Wave 10 — Bootstrap & Deployment

**Goal:** `bootstrap.sh` installs Stronghold on a fresh Vultr box. systemd units work. Workers can be added.

**Subagent:** `general-purpose` for shell scripts. Orchestrating agent for systemd units (security hardening).

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W10-T1 | Real `bootstrap.sh` | `setup/bootstrap.sh` | Idempotent. Installs Rust, builds binary, initializes DB, generates keys, installs systemd, starts services, prints summary. Works on Rocky 9 and 10. | Integration: run on fresh Vultr box → all services running, gateway reachable on 8443. |
| W10-T2 | Real `worker-bootstrap.sh` | `setup/worker-bootstrap.sh` | Installs k3s worker, ntfy, registry. Joins cluster. Opens firewall. | Integration: run on fresh box → k3s agent active, registered with control plane. |
| W10-T3 | systemd unit hardening | `setup/systemd/*.service` | All units have security directives: `NoNewPrivileges`, `ProtectSystem`, `PrivateTmp`, etc. Gateway unit allows `/dev/sev` (not `PrivateDevices=true`). | Unit file review. `systemd-analyze security` score <5.0. |
| W10-T4 | ntfy server configuration | `setup/ntfy.yml` | ACL'd topics per tenant. Auth required. Attachments disabled. Rate limits. | Integration: subscribe without auth → 401. With auth → receives pushes. |
| W10-T5 | Firewall configuration | `setup/firewall.sh` (new) | 8443 (gateway) and 8090 (ntfy) open publicly. 6443 (k3s), 10250 (kubelet), 5000 (registry) open only on Tailscale interface. | Integration: `nmap` from external → only 8443, 8090 open. |
| W10-T6 | Tailscale integration | `setup/tailscale.sh` (new) | Optional: install Tailscale, configure to only expose gateway ports. | Integration: install → box on tailnet → workers reachable via Tailscale IP. |
| W10-T7 | Backup script | `setup/backup.sh` (new) | `stronghold backup --to s3://...`. Encrypts keys with tenant password. SQLite online backup. | Integration: run backup → tarball in S3 → restore on fresh box → state matches. |
| W10-T8 | Upgrade script | `setup/upgrade.sh` (new) | `stronghold upgrade`: pulls new binary, verifies signature, drains, restarts, re-attests SEV-SNP, rotates keys. | Integration: upgrade from v1.0 to v1.1 → audit log still verifies, new measurement recorded. |
| W10-T9 | Health check endpoint + monitoring | `setup/monitoring.sh` (new) | Prometheus metrics at `/metrics`. Grafana dashboard JSON. Alert rules. | Integration: scrape metrics → dashboard shows gateway health. |
| W10-T10 | Multi-box deployment runbook | `docs/DEPLOYMENT.md` | Step-by-step for single-box, multi-box, community-hosted. Includes troubleshooting. | Manual: follow runbook on fresh Vultr account → working Stronghold. |

**Wave 10 DoD:**
- [ ] Fresh Vultr box → working Stronghold in <15 minutes via `bootstrap.sh`
- [ ] Workers addable in <5 minutes
- [ ] Backup/restore tested end-to-end
- [ ] Upgrade path tested (v1.0 → v1.1)
- [ ] systemd security hardening verified

---

### Wave 11 — Integration & E2E

**Goal:** Full end-to-end tests pass. Agent can ORDER, get PTY, run commands, RELEASE. Phone approves. Audit log verifies.

**Subagent:** `general-purpose` for E2E test harness. Orchestrating agent for test design.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W11-T1 | E2E test harness | `tests/e2e/` (new) | Python `pytest` + `paramiko`. Spins up gateway, simulates agent, drives phone via Playwright. | Harness runs end-to-end on dev box. |
| W11-T2 | E2E: full session lifecycle | `tests/e2e/test_session_lifecycle.py` | ORDER → approve → PTY → run `echo hello` → receive `hello` → RELEASE. Audit log has all entries. | Test passes on dev box (no-sev-snp mode). |
| W11-T3 | E2E: resume after disconnect | `tests/e2e/test_resume.py` | ORDER → PTY → disconnect → RESUME → same machine, same files. | Test passes. |
| W11-T4 | E2E: extend session | `tests/e2e/test_extend.py` | ORDER (1h TTL) → at 30min, EXTEND → approve → TTL extended. | Test passes. |
| W11-T5 | E2E: revoke from phone | `tests/e2e/test_revoke.py` | ORDER → PTY → phone taps REVOKE → PTY closes within 500ms. | Test passes. |
| W11-T6 | E2E: quorum for destructive op | `tests/e2e/test_quorum.py` | Session active → run `rm -rf /tmp/test` → blocks → 1st phone approves → blocks → 2nd phone approves → executes. | Test passes with 2 enrolled credentials. |
| W11-T7 | E2E: anomaly push | `tests/e2e/test_anomaly.py` | Run `curl evil.com` in PTY → anomaly push received on phone within 2s. | Test passes. |
| W11-T8 | E2E: audit verification | `tests/e2e/test_audit.py` | After session, `stronghold audit verify` passes. Tamper one entry → verify fails. | Test passes. |
| W11-T9 | E2E: multi-tenant isolation | `tests/e2e/test_multi_tenant.py` | Tenant A's pod cannot reach tenant B's pod. Tenant A cannot read tenant B's audit log. | Test passes. |
| W11-T10 | E2E: SEV-SNP attestation | `tests/e2e/test_sev_snp.py` (skipped on dev box) | On SEV box: enroll with matching measurement → OK. Tamper binary → enrollment refused. | Test passes on SEV box, skips on dev box. |
| W11-T11 | Load test: 100 concurrent sessions | `tests/load/test_100_sessions.py` | 100 agents ORDER simultaneously. All get sessions. PTY streams work. No errors. | Test passes. Documented throughput. |
| W11-T12 | Load test: audit log throughput | `tests/load/test_audit_throughput.py` | 10,000 audit entries in <10s. Verify all. | Test passes. Documented throughput. |
| W11-T13 | CI pipeline | `.github/workflows/ci.yml` | Build, test, clippy, fmt, audit, deny on every PR. Matrix: Rocky 9, Rocky 10, Ubuntu 24.04. | CI green on PR. |
| W11-T14 | Coverage reporting | `.github/workflows/coverage.yml` | `cargo tarpaulin` → codecov.io. PR comments with coverage diff. | Coverage report posted on PR. |
| W11-T15 | Release pipeline | `.github/workflows/release.yml` | On tag: build binaries for x86_64 + aarch64. Sign with cosign. Publish to GitHub Releases. | Release artifacts on tag push. |

**Wave 11 DoD:**
- [ ] All E2E tests pass on dev box (no-sev-snp mode)
- [ ] All E2E tests pass on SEV-SNP box (sev-snp mode)
- [ ] Load tests pass with documented throughput
- [ ] CI pipeline green on main
- [ ] Coverage >80% overall

---

### Wave 12 — Hardening & Release

**Goal:** v1.0.0 release. Security audit passed. Docs complete. Release notes written.

**Subagent:** `general-purpose` for docs. `ppt-expert` for release presentation. Orchestrating agent for security review.

| ID | Title | Files | DoD | Tests |
|---|---|---|---|---|
| W12-T1 | Security self-audit | `docs/SECURITY_AUDIT.md` (new) | Review every crypto call site. Review every SQL query. Review every unsafe block. Review systemd units. Document findings. | Audit doc complete. All findings addressed or documented as accepted risk. |
| W12-T2 | Third-party dependency audit | `Cargo.lock` | `cargo audit` clean. `cargo deny check` clean. No GPL/AGPL dependencies (license compatibility). | Reports clean. |
| W12-T3 | Threat model validation | `docs/THREAT_MODEL.md` | Each threat in the model has a corresponding test proving mitigation. | All threats tested. |
| W12-T4 | Documentation review | `docs/` | All docs reviewed for accuracy. No stale info. Examples work. | Manual review. |
| W12-T5 | API documentation | `docs/API.md` (new) | OpenAPI spec for all HTTP endpoints. Generated from code (`utoipa` crate) or hand-written. | Spec validates. |
| W12-T6 | Operations runbook | `docs/OPERATIONS.md` | Covers: install, upgrade, backup, restore, key rotation, credential revocation, troubleshooting. | Runbook tested by following it on fresh box. |
| W12-T7 | Release notes | `CHANGELOG.md` + `docs/releases/v1.0.0.md` | Comprehensive release notes. Features, known issues, upgrade path from scaffold. | Notes reviewed. |
| W12-T8 | Binary signing | `.github/workflows/release.yml` | Release binaries signed with cosign + GPG. Sigstore bundle published. | `cosign verify` passes. |
| W12-T9 | Measurement registry | `docs/MEASUREMENTS/v1.0.txt` | Real SEV-SNP measurement for v1.0.0. GPG-signed. | Measurement matches running gateway. Signature verifies. |
| W12-T10 | Release presentation | `docs/releases/v1.0.0-slides.html` | 10-slide deck for v1.0.0 release. Architecture, features, demo. | `ppt-expert` produces standalone HTML slides. |
| W12-T11 | v1.0.0 tag + GitHub release | repo | Tag `v1.0.0` pushed. GitHub release published with binaries, signatures, measurement. | Release page live. |
| W12-T12 | Post-release monitoring | infra | Monitor for 7 days. Track issues. Patch release if needed. | No critical issues for 7 days. |

**Wave 12 DoD:**
- [ ] Security audit complete, findings addressed
- [ ] All docs accurate and complete
- [ ] Release binaries signed and published
- [ ] SEV-SNP measurement registered
- [ ] v1.0.0 tagged and released
- [ ] 7 days post-release with no critical issues

---

## Definition of Done — Project Level

Stronghold v1.0.0 is done when ALL of the following are true:

### Functional
- [ ] Agent can ORDER a machine, get PTY, run commands, RELEASE
- [ ] Phone approves/denies/revokes sessions
- [ ] Multiple agents work concurrently on different projects
- [ ] Multi-tenant isolation verified (no cross-tenant access)
- [ ] VPS escalation works for GPU/large-memory workloads
- [ ] Audit log is dual-signed, hash-chained, verifiable offline
- [ ] Key rotation works without losing historical verifiability

### Security
- [ ] TLS 1.3 + X25519Kyber768 hybrid transport
- [ ] Ed25519 + ML-DSA-65 dual-signed audit log
- [ ] X25519 + ML-KEM-768 E2E-encrypted push notifications
- [ ] WebAuthn with biometric verification for all approvals
- [ ] SEV-SNP attestation verifiable by phone before enrollment
- [ ] Keys sealed to launch measurement
- [ ] Quorum for destructive operations
- [ ] No external providers for content (ntfy self-hosted, APNs/FCM wake-up only)
- [ ] No custom phone app (browser + ntfy only)
- [ ] `cargo audit` clean
- [ ] `cargo deny` clean
- [ ] Security audit passed

### Quality
- [ ] 80%+ overall line coverage
- [ ] 90%+ line coverage in `crypto/`, `audit/`, `tenants/auth/`
- [ ] All NIST KAT vectors pass
- [ ] `cargo fuzz` 1M iterations, no panics
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] All E2E tests pass on dev box and SEV-SNP box
- [ ] Load test: 100 concurrent sessions, no errors

### Documentation
- [ ] README accurate and complete
- [ ] All ADRs finalized
- [ ] Threat model validated against tests
- [ ] Operations runbook tested
- [ ] Deployment guide covers single-box, multi-box, community-hosted
- [ ] SEV-SNP guide complete
- [ ] Image DSL guide complete
- [ ] API documentation (OpenAPI) complete
- [ ] Release notes written

### Release
- [ ] v1.0.0 tag pushed
- [ ] Binaries published for x86_64 + aarch64
- [ ] Binaries signed with cosign + GPG
- [ ] SEV-SNP measurement registered and signed
- [ ] GitHub release published
- [ ] 7 days post-release monitoring, no critical issues

---

## Appendix: Task Dependency Graph

```
Wave 0 (compile) ──┐
                   ├─→ Wave 1 (crypto) ──┐
                   │                      ├─→ Wave 2 (db/tenants) ──┐
                   │                      │                          ├─→ Wave 3 (sessions/machines) ──┐
                   │                      │                          │                                  ├─→ Wave 4 (routes/PTY) ──┐
                   │                      │                          │                                  │                          ├─→ Wave 5 (audit/push) ──┐
                   │                      │                          │                                  │                          │                          ├─→ Wave 6 (images) ──┐
                   │                      │                          │                                  │                          │                          │                      ├─→ Wave 8 (phone/PWA) ──┐
                   │                      │                          │                                  │                          │                          │                      │                          ├─→ Wave 9 (CLI) ──┐
                   │                      │                          │                                  │                          │                          │                      │                          │                    ├─→ Wave 10 (bootstrap) ──┐
                   │                      │                          │                                  │                          │                          │                      │                          │                    │                            ├─→ Wave 11 (E2E) ──→ Wave 12 (release)
                   │                      │                          │                                  │                          │                          │                      │                          │                    │                            │
                   Wave 7 (SEV-SNP) ───────────────────────────────────────────────────────────────────┴──────────────────────────┴────────────────────────────┴────┘
```

**Critical path:** W0 → W1 → W2 → W3 → W4 → W11 → W12.

**Parallelizable:** W5, W6, W7, W8, W9, W10 can all run in parallel after W4.

---

## Appendix: Estimated Effort

| Wave | Tasks | Est. Hours | Est. Days (1 dev) |
|---|---|---|---|
| 0 | 15 | 8 | 1 |
| 1 | 11 | 40 | 5 |
| 2 | 10 | 24 | 3 |
| 3 | 15 | 48 | 6 |
| 4 | 15 | 32 | 4 |
| 5 | 10 | 32 | 4 |
| 6 | 10 | 24 | 3 |
| 7 | 9 | 32 | 4 |
| 8 | 10 | 24 | 3 |
| 9 | 10 | 16 | 2 |
| 10 | 10 | 24 | 3 |
| 11 | 15 | 40 | 5 |
| 12 | 12 | 24 | 3 |
| **Total** | **152** | **368** | **46** |

With one developer: ~10 weeks. With parallel subagents on parallelizable waves: ~6 weeks.

---

*Last updated: 2026-07-29*
*Maintained by: Stronghold Contributors*
