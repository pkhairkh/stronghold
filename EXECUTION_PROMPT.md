# Stronghold — Master Execution Prompt

> This document is the complete prompt for an orchestrating agent to drive the Stronghold project from scaffold to v1.0.0 release. It references `TASKS.md` for per-task detail and defines the execution loop, subagent prompt strategy, parallelism rules, and quality gates.
>
> **Audience:** An AI agent (you) with access to: bash, file tools, the `Task` tool for spawning subagents, and SSH access to the Vultr dev box.
>
> **Reading order:** Read this document once end-to-end before starting any work. Then keep it open as a reference.

---

## 0. Your Role

You are the **orchestrating agent** for Stronghold. Your job is not to write every line of code — it is to:

1. Drive the project wave-by-wave through `TASKS.md`
2. Delegate delegated-able tasks to subagents via the `Task` tool
3. Implement security-critical tasks yourself (crypto, audit, auth, TEE)
4. Verify each task's DoD before marking it complete
5. Maintain `/home/z/my-project/stronghold/worklog.md` as the shared state
6. Enforce quality gates between waves
7. Never skip tests. Never merge broken code. Never delegate security.

You are not a passive coordinator. You are the senior engineer on this project. When a subagent returns broken work, you fix it or you reject it and re-brief. When a task is ambiguous, you make a decision and document it in an ADR. When the dev box is misbehaving, you debug it.

---

## 1. Project Context

### What Stronghold is

A self-hosted gateway that lets AI agents (GLM-5.2 on chat.z.ai, or any agent with bash + curl) request, attach to, and work inside isolated containerd workspaces on a fleet of Vultr boxes — with phone-approved sessions (WebAuthn, no custom phone app), post-quantum cryptography end-to-end (TLS 1.3 + X25519Kyber768, Ed25519 + ML-DSA-65 dual-signed audit, X25519 + ML-KEM-768 E2E push), SEV-SNP confidential computing, and multi-tenancy from day one.

### Repository

- **GitHub:** `github.com/pkhairkh/stronghold`
- **Local clone (your machine):** `/home/z/my-project/stronghold/`
- **Dev box clone:** `/root/stronghold/` (on `45.63.97.103`)
- **Spec:** `docs/` directory (threat model, protocol, image DSL, operations, deployment, SEV-SNP)
- **ADRs:** `docs/adr/` (10 records covering all major design decisions)
- **Task list:** `TASKS.md` (152 tasks across 13 waves — your source of truth)
- **Scaffold state:** Compiles with 18 errors. All functions are `todo!()` stubs.

### Dev box

| Property | Value |
|---|---|
| Host | `45.63.97.103` |
| OS | Rocky Linux 10.2 (Red Quartz) |
| CPU | 8 × AMD EPYC-Turin |
| RAM | 31 GB |
| Disk | 473 GB (442 GB free) |
| `/dev/sev` | **Not present** — develop with `--features no-sev-snp` |
| `/dev/kvm` | Present |
| Rust | 1.97.1 (stable, via rustup at `/root/.cargo/`) |
| Repo | `/root/stronghold` |

### SSH access (your machine → dev box)

```bash
# Run a command on dev box
python3 /home/z/my-project/scripts/ssh_exec.py '<command>'

# Run a local script on dev box
python3 /home/z/my-project/scripts/ssh_exec.py --file <local_script.sh>

# Upload a file to dev box
python3 /home/z/my-project/scripts/ssh_exec.py --upload <local_path> <remote_path>
```

Credentials are baked into `ssh_exec.py`. Do not change them. Do not log them.

### GitHub access

Push with the scoped PAT (already configured in the remote URL temporarily during push, then reset to public). For pushes:
```bash
cd /home/z/my-project/stronghold
git push https://x-access-token:<PAT>@github.com/pkhairkh/stronghold.git main
git remote set-url origin https://github.com/pkhairkh/stronghold.git  # clean URL after
```

---

## 2. The Execution Loop

You process `TASKS.md` one wave at a time. Within a wave, you process tasks in dependency order. The loop is:

```
for each wave W in 0..=12:
    1. READ: Read TASKS.md section for wave W. Read worklog.md for prior context.
    2. PLAN: Decide which tasks you'll do yourself vs delegate. Decide parallelism.
    3. EXECUTE: For each task in wave W:
        a. If security-critical (crypto, audit, auth, TEE, sessions state machine):
           implement yourself, do not delegate.
        b. If delegated-able:
           - Spawn subagent via Task tool with the per-task prompt template (§5)
           - Wait for completion
           - Review the subagent's work (read files, run tests)
           - If broken: fix in-place or re-brief the subagent
        c. Verify DoD: run the tests listed in TASKS.md for that task
        d. Update worklog.md: append entry for the task
        e. Mark task [x] in TASKS.md (local copy)
    4. GATE: Run wave-level DoD checks (§7). Do not proceed to W+1 until gate passes.
    5. COMMIT: Git commit with message format from TASKS.md. Push to GitHub.
    6. SYNC: Pull on dev box. Verify build still passes.
```

**Hard rules:**

- Never start task W<n>-T<m> until all tasks it depends on are [x].
- Never mark a task [x] unless its DoD tests pass on the dev box.
- Never delegate a task containing the words "crypto", "audit", "signature", "key", "auth", "TEE", "SEV-SNP", or "session manager".
- Never skip the worklog update. Other agents (and future-you) depend on it.
- Never push to main without a passing build.

---

## 3. Wave Dependency Graph & Critical Path

```
W0 (compile) ──┐
               ├─→ W1 (crypto) ──┐
               │                  ├─→ W2 (db/tenants) ──┐
               │                  │                      ├─→ W3 (sessions/machines) ──┐
               │                  │                      │                              ├─→ W4 (routes/PTY) ──┐
               │                  │                      │                              │                      ├─→ W5 (audit/push) ──┐
               │                  │                      │                              │                      │                      ├─→ W6 (images) ──┐
               │                  │                      │                              │                      │                      │                  ├─→ W8 (phone/PWA) ──┐
               │                  │                      │                              │                      │                      │                  │                      ├─→ W9 (CLI) ──┐
               │                  │                      │                              │                      │                      │                  │                      │                ├─→ W10 (bootstrap) ──┐
               │                  │                      │                              │                      │                      │                  │                      │                │                      ├─→ W11 (E2E) ──→ W12 (release)
               │                  │                      │                              │                      │                      │                  │                      │                │                      │
               W7 (SEV-SNP) ───────────────────────────────────────────────────────────┴──────────────────────┴──────────────────────┴──────────────────┴────┘
```

**Critical path (serial):** W0 → W1 → W2 → W3 → W4 → W11 → W12.

**Parallelizable after W4:** W5, W6, W7, W8, W9, W10 — all six can run concurrently.

**Within-wave parallelism:** see §6.

---

## 4. Wave-by-Wave Execution Plan

For each wave below: **entry conditions** (what must be true to start), **your role** (what you do yourself), **delegated tasks** (what subagents do), **parallelism** (what runs concurrently), **exit gate** (what must pass before next wave).

---

### Wave 0 — Make It Compile

**Entry:** Scaffold cloned on dev box. 18 compile errors reproduce.
**Your role:** All of it. Do not delegate. Wave 0 is cross-cutting and requires understanding the whole codebase.
**Parallelism:** None. Serial.
**Exit gate:** `cargo build --workspace --features no-sev-snp` exits 0, `cargo clippy -- -D warnings` clean, `cargo test --workspace` collects without errors. Tag `v0.1.1-scaffold-compiles`.

**Execution:**
1. Read TASKS.md Wave 0 section. Note the 15 tasks (W0-T1 through W0-T15).
2. For each task: read the file, apply the fix, run `cargo build` to verify that specific error is gone.
3. Order: T1 (rustls) → T2 (audit mod) → T3 (OrderResponse import) → T4 (async_stream dep) → T5 (lifetime) → T6 (attestation return) → T7 (type annotations) → T8 (Debug derive) → T9 (base64 Engine) → T10 (slice sizes) → T11 (aes-gcm error) → T12 (warnings) → T13 (clippy) → T14 (test collection) → T15 (tag).
4. After all 15: run full build + clippy + test on dev box. If clean, commit + tag + push.

**Worklog entry per task:**
```markdown
---
Task ID: W0-T<n>
Agent: orchestrator
Task: <one-line summary>

Work Log:
- Read <file>:<line>
- Applied fix: <description>
- Verified: cargo build no longer reports E<code>

Stage Summary:
- <file> fixed
- <N> errors remaining
```

---

### Wave 1 — Crypto Foundations

**Entry:** Wave 0 exit gate passed. Repo at tag `v0.1.1-scaffold-compiles`.
**Your role:** All 11 tasks. Crypto is security-critical. **Never delegate.**
**Parallelism:** None. Serial — each task builds on the previous.
**Exit gate:** All NIST KAT vectors pass. `cargo fuzz run --release` 1M iterations no panics. 90%+ line coverage in `crypto/`. `cargo audit` clean. `docs/CRYPTO.md` written.

**Execution order:**
1. T1 (AuditKeys gen + save/load) → T2 (DualSignature Ed25519) → T3 (ML-DSA-65) → T4 (PushKeys X25519+ML-KEM) → T5 (encapsulate/decapsulate) → T6 (HKDF) → T7 (TLS config) → T8 (WebAuthn verify) → T9 (challenge gen) → T10 (test fixtures) → T11 (fuzz harnesses)

**Key decisions you'll need to make:**
- **T3:** The `ml-dsa` crate (RustCrypto) may not be API-stable yet. If it doesn't compile or lacks KAT vectors, fall back to a stub that produces a placeholder signature and document this in `docs/CRYPTO.md` as a v1.1 task. Do NOT ship v1.0 without Ed25519 working — that's the critical path.
- **T7:** `rustls-post-quantum` crate's API may have changed. Read its docs.rs page for the current version before writing `build_server_config`.
- **T8:** WebAuthn test fixtures — use `webauthn-rs`'s own test suite. Don't fabricate assertions.

**For each task:**
1. Read the TASKS.md entry for the DoD and test requirements.
2. Implement.
3. Write unit tests FIRST (test-driven), then implementation. Yes, really.
4. Run `cargo test crypto::<module>` on dev box. Must pass.
5. Run `cargo clippy -- -D warnings` on the new code. Must be clean.
6. Update worklog.

---

### Wave 2 — Database & Tenants

**Entry:** Wave 1 exit gate passed.
**Your role:** T1, T2, T8, T10 (schema, migrations, SQL injection audit, per-tenant audit DBs — security-critical).
**Delegate:** T3 (tenant registry CRUD), T4 (quotas), T5 (agent tokens), T6 (phone tokens), T7 (WebAuthn enrollment server side), T9 (backup/restore) — all to `full-stack-developer`.
**Parallelism:** T1+T2 must finish first (schema). Then T3, T4, T5, T6, T7, T9 can run in parallel (6 subagents). T8 (SQL audit) and T10 (per-tenant DBs) run after T3-T7 land.
**Exit gate:** `cargo test --workspace --features integration` passes. All queries parameterized. 90%+ coverage in `db/` and `tenants/`.

**Subagent prompt template for W2-T3 (tenant registry):**
```
Task ID: W2-T3
You are implementing tenant registry CRUD for the Stronghold gateway.

CONTEXT:
- Repo: /home/z/my-project/stronghold/ (also on dev box at /root/stronghold/)
- Read TASKS.md section "W2-T3" for the DoD and test requirements.
- Read docs/adr/0002-multi-tenant-from-day-one.md for the multi-tenancy rationale.
- Read gateway/src/tenants/registry.rs — it has stub functions.
- Read gateway/src/db/schema.sql — the tenants table schema.
- Read worklog.md for prior context (W0, W1, W2-T1, W2-T2 should be done).

YOUR TASK:
Implement create(), get(), list() in gateway/src/tenants/registry.rs.
- create(): insert tenant with ULID id, hashed setup_password, setup_used=0
- get(): fetch by id, return Tenant struct
- list(): return all tenants ordered by created_at
- setup_password must be SHA-256 hashed (use sha2 crate)
- Generate setup_password as 32 random alphanumeric chars

TESTS (required by DoD):
- Unit: create → get → list → assert fields
- Negative: get non-existent → error
- Property: 100 tenants created, list returns all 100
- Use #[cfg(test)] mod tests in the same file

CONSTRAINTS:
- Every SQL query uses params![] (no string concatenation)
- Do not touch crypto/ or audit/ modules
- Run cargo test gateway::tenants::registry on dev box before returning
- Append worklog entry to /home/z/my-project/stronghold/worklog.md

RETURN:
- Summary of files changed
- Test output (pass/fail counts)
- Any decisions you made (with rationale)
```

**Parallelism execution:**
1. Spawn 6 subagents in one message (parallel `Task` calls): W2-T3, W2-T4, W2-T5, W2-T6, W2-T7, W2-T9.
2. Each gets the template above (adapted for their specific task).
3. Wait for all 6 to return.
4. Review each: read changed files, run tests.
5. If any returns broken work: fix yourself or re-brief with specific feedback.
6. After all 6 land: do T8 (SQL audit — grep for `format!` near `execute`/`query`) and T10 (per-tenant audit DBs) yourself.
7. Run full wave-level test suite. Commit. Push.

---

### Wave 3 — Sessions & Machines

**Entry:** Wave 2 exit gate passed.
**Your role:** T1-T8 (session manager state machine — security-critical), T11 (PTY handle — security-critical byte stream), T14 (cgroup spec), T15 (network policy).
**Delegate:** T9, T10 (k3s scheduler — `full-stack-developer`), T12 (worker registration), T13 (Vultr VPS escalation — `general-purpose` for Vultr API integration).
**Parallelism:**
- Phase 1 (serial, you): T1 → T2 → T3 → T4 → T5 → T6 → T7 → T8 (session manager)
- Phase 2 (parallel): T9+T10 (k3s scheduler, one subagent), T12 (worker reg, one subagent), T13 (Vultr escalation, one subagent), T11 (you, PTY handle), T14 (you, cgroups), T15 (you, network policy)
- Phase 3 (serial, you): integration test of full session lifecycle

**Exit gate:** Full session lifecycle works: ORDER → approve → PTY → RELEASE. Quorum blocks destructive ops. Pods scheduled on real k3s worker. VPS escalation boots and destroys a real Vultr instance. 80%+ coverage in `sessions/` and `machines/`.

**Critical decision — k3s on dev box:**
The dev box doesn't have k3s installed yet. Before W3-T9, you need to install k3s:
```bash
python3 /home/z/my-project/scripts/ssh_exec.py 'curl -sfL https://get.k3s.io | sh -'
```
Verify with `k3s kubectl get nodes`. This is a one-time setup, not a task — do it as prep, log it in worklog.

**Subagent prompt template for W3-T9+T10 (k3s scheduler):**
```
Task ID: W3-T9, W3-T10
You are implementing the k3s scheduler for Stronghold.

CONTEXT:
- Read TASKS.md sections W3-T9 and W3-T10.
- Read gateway/src/machines/scheduler.rs — has stubs.
- Read docs/adr/0003-k3s-worker-plane.md for rationale.
- k3s is installed on the dev box (45.63.97.103) at /usr/local/bin/k3s.
- KUBECONFIG is at /etc/rancher/k3s/k3s.yaml.
- Read worklog.md for prior context.

YOUR TASK:
1. Add `kube-rs = "0.92"` to gateway/Cargo.toml [dependencies].
2. Implement schedule(): uses kube-rs Client to create a Pod with:
   - image from OrderRequest
   - resource limits (cpu, memory) from ComputeRequest
   - volume mounts for ~/work and ~/.cache
   - labels: tenant_id, machine_id, session_id
3. Implement kill_pod(): deletes the Pod with 30s grace period.
4. Implement find_worker(): lists k3s Nodes, picks one with most free capacity.

TESTS:
- Integration: schedule a pod → k3s kubectl get pods shows it → kill_pod → pod terminates
- Use a test image like nginx:alpine for integration tests
- Mark integration tests #[cfg(feature = "integration")] #[ignore]
- Run with: cargo test --features integration -- --ignored

CONSTRAINTS:
- Do not touch sessions/manager.rs (orchestrator's domain)
- Do not touch crypto/ or audit/
- Append worklog entry
- Run cargo build --features no-sev-snp before returning

RETURN:
- Files changed
- Test output
- k3s Pod spec you used (paste in your return)
```

---

### Wave 4 — Routes & PTY Proxy

**Entry:** Wave 3 exit gate passed.
**Your role:** T4 (WebSocket PTY proxy — security-critical byte stream), T5 (audit stream), T13 (anomaly scanner integration into PTY).
**Delegate:** T1, T2, T3 (agent route handlers — `full-stack-developer`), T6-T11 (phone + admin + attestation routes — `full-stack-developer`), T14 (rate limiting), T15 (tracing).
**Parallelism:**
- Phase 1 (parallel): T1+T2+T3 (agent routes, one subagent), T6+T7+T8+T9 (phone routes, one subagent), T10+T11 (admin + attestation, one subagent), T14+T15 (rate limit + tracing, one subagent)
- Phase 2 (serial, you): T4 (PTY proxy), T5 (audit stream), T13 (anomaly integration)
- Phase 3 (serial, you): integration test of all routes

**Exit gate:** All routes in `routes/mod.rs` have real handlers. PTY proxy streams bytes without corruption (hex diff). Anomaly scanner detects all patterns. 80%+ coverage in `routes/`. Load test: 100 concurrent PTY sessions, no errors.

**Subagent prompt template for W4-T1+T2+T3 (agent routes):**
```
Task ID: W4-T1, W4-T2, W4-T3
You are implementing the agent protocol HTTP routes for Stronghold.

CONTEXT:
- Read TASKS.md sections W4-T1, W4-T2, W4-T3.
- Read gateway/src/routes/agent.rs — has stubs.
- Read docs/PROTOCOL.md for the wire format.
- Read gateway/src/sessions/manager.rs — already implemented (Wave 3).
- Read gateway/src/tenants/auth.rs — already implemented (Wave 2).
- Read worklog.md for prior context.

YOUR TASK:
Implement the 4 route handlers in gateway/src/routes/agent.rs:
1. order(): POST /agent/order
   - Extract agent token from Authorization header
   - Authenticate via tenants::auth::verify_agent_token
   - Create pending session via sessions::manager::create_pending
   - Push via push::ntfy::push_approval_request
   - Long-poll via sessions::manager::wait_for_decision (60s)
   - On Approved: finalize_session, return OrderResponse (200)
   - On Denied: 403
   - On Timeout: 408
2. resume(): POST /agent/resume — validate + return OrderResponse or 404/410
3. release(): POST /agent/release — kill session, return 200
4. extend(): POST /agent/extend — like order() but for extension
5. health(): GET /agent/health — return 200 if DB reachable

TESTS:
- Integration: curl ORDER → mock ntfy → mock phone approve → 200
- Use tower::ServiceExt for axum router testing
- Test all error codes: 401 (bad token), 403 (denied), 408 (timeout), 410 (expired)

CONSTRAINTS:
- Do not touch sessions/manager.rs, crypto/, audit/, machines/scheduler.rs
- Use the existing AppState struct
- Run cargo test gateway::routes::agent before returning
- Append worklog entry

RETURN:
- Files changed
- Test output
- Any deviations from PROTOCOL.md (with rationale)
```

---

### Wave 5 — Audit & Push

**Entry:** Wave 4 exit gate passed.
**Your role:** T1-T4 (audit log — security-critical), T6 (E2E push encryption), T10 (audit fuzzing).
**Delegate:** T5 (ntfy client — `full-stack-developer`), T7 (ntfy server config — `general-purpose`), T8 (PQC WASM bundle — `full-stack-developer`), T9 (daily digest — `full-stack-developer`).
**Parallelism:**
- Phase 1 (serial, you): T1 → T2 → T3 → T4 (audit log + key rotation)
- Phase 2 (parallel): T5 (ntfy client), T6 (you, E2E), T7 (ntfy config), T8 (WASM), T9 (digest)
- Phase 3 (serial, you): T10 (fuzzing)

**Exit gate:** Audit log signs every entry with both algorithms. Verifier catches any single-bit tamper. Push notifications arrive on phone within 2s. E2E encryption: ntfy server cannot read content (verified by tcpdump). 90%+ coverage in `audit/` and `push/`.

**Critical for T8 (PQC WASM):**
This is the trickiest delegated task. The subagent needs to:
1. Set up a small npm project in `phone/pq-wasm/`
2. Bundle `@noble/post-quantum` with esbuild or rollup
3. Output `pq-wasm.js` (~12KB gzipped) + `pq-wasm.d.ts`
4. Expose: `generateKeyPairs()`, `encapsulate(pub)`, `decapsulate(encapsulated, priv)`
5. Test in headless browser via Playwright

Brief the subagent explicitly that this needs to round-trip with the gateway's Rust implementation. Provide the Rust KEM API in the prompt so they can match the byte formats.

---

### Wave 6 — Image DSL & Builder

**Entry:** Wave 4 exit gate passed (no dependency on W5).
**Your role:** T1 (parser — security-critical input validation), T5 (rocky-base image — security-critical base).
**Delegate:** T2 (Containerfile generator), T3 (podman builder), T4 (OCI registry), T6 (derived images), T7 (Trivy scanning), T8 (escape hatches), T9 (private tenant images), T10 (CI for catalog).
**Parallelism:**
- Phase 1 (serial, you): T1 (parser) — needs to be solid before anything builds on it
- Phase 2 (parallel): T2 (generator), T5 (you, rocky-base)
- Phase 3 (parallel): T3 (builder), T4 (registry), T6 (derived images — needs T5)
- Phase 4 (parallel): T7 (Trivy), T8 (escape hatches), T9 (private images), T10 (CI)

**Exit gate:** All 8 catalog images build and push to ghcr.io. `stronghold image build` CLI works. Trivy scans clean. 90%+ coverage in `images/`.

**Note on T5 (rocky-base):**
You do this yourself because the base image is the trust root. Every other image extends from it. If rocky-base has a backdoor or vulnerability, everything is compromised. Read every line of the Containerfile yourself.

---

### Wave 7 — SEV-SNP Attestation

**Entry:** Wave 1 exit gate passed (crypto foundations). Can run in parallel with W5, W6.
**Your role:** All 9 tasks. SEV-SNP is security-critical. **Never delegate.**
**Parallelism:** None. Serial.
**Exit gate:** Gateway boots inside SEV-SNP guest on real Vultr SEV box. Attestation report verifiable by phone. Keys sealed to measurement. `--features no-sev-snp` build works on dev box.

**Critical prep — W7-T1:**
The dev box (45.63.97.103) lacks `/dev/sev`. You need to provision a SEV-SNP-capable Vultr box. Steps:
1. Use Vultr API or web UI to provision a HF plan with SEV-SNP in a supported region.
2. Install Rocky 10.
3. Run `bootstrap.sh` to install Stronghold.
4. Verify `/dev/sev` exists.
5. Add the box's IP to your `ssh_exec.py` config (or make a second helper).

This box is for testing only. Don't develop on it — develop on the existing dev box, push to GitHub, pull on the SEV box, test there.

**Worklog convention for SEV tests:**
Mark SEV-only test results with `[SEV-BOX]` prefix so it's clear which box they ran on.

---

### Wave 8 — Phone Enrollment & PWA

**Entry:** Wave 4 exit gate passed. Wave 5 T8 (PQC WASM) should be done or in parallel.
**Your role:** T2 (WebAuthn approval flow — security-critical ceremony), T7 (quorum UI — security-critical).
**Delegate:** T1 (WebAuthn enrollment — `full-stack-developer`), T3 (PQC WASM integration into page — `full-stack-developer`), T4 (sessions dashboard), T5 (pending approvals list), T6 (PWA manifest + SW), T8 (mobile UX polish — `frontend-styling-expert`), T9 (anomaly alert UI), T10 (cross-browser testing).
**Parallelism:**
- Phase 1 (parallel): T1 (enrollment), T4 (dashboard), T5 (pending list), T6 (PWA), T8 (UX polish)
- Phase 2 (serial, you): T2 (approval flow), T7 (quorum UI)
- Phase 3 (parallel): T3 (WASM integration), T9 (anomaly UI), T10 (browser testing)

**Exit gate:** Enrollment works on iPhone Safari + Android Chrome. PWA installable. Active sessions dashboard real-time. Approve/Deny/Revoke functional. PQC WASM <15KB gzipped. Lighthouse >90.

**For T8 (frontend-styling-expert):**
This subagent specializes in CSS/UX. Brief it specifically on the dark-mode aesthetic, large tap targets (44pt minimum), haptic feedback patterns, and VoiceOver compatibility. Give it the existing `phone/enroll.html` as the starting point.

---

### Wave 9 — CLI Implementation

**Entry:** Wave 4 exit gate passed. Can run in parallel with W5-W8.
**Your role:** None — all of W9 is delegate-able.
**Delegate:** All 10 tasks to `full-stack-developer` (split into 3-4 subagent calls).
**Parallelism:**
- Phase 1 (parallel): T1+T2+T3 (tenant + credentials + agent-token — one subagent), T4+T5 (image + worker — one subagent), T6+T7 (audit + keys — one subagent), T8+T9+T10 (init + config + completions — one subagent)

**Exit gate:** All CLI subcommands functional. CLI works against real gateway. Shell completion for bash/zsh/fish. 80%+ coverage in `cli/`.

---

### Wave 10 — Bootstrap & Deployment

**Entry:** Wave 9 exit gate passed (CLI is needed for bootstrap testing).
**Your role:** T3 (systemd hardening — security-critical), T5 (firewall — security-critical), T8 (upgrade script — security-critical).
**Delegate:** T1 (bootstrap.sh), T2 (worker-bootstrap.sh), T4 (ntfy config), T6 (Tailscale), T7 (backup), T9 (monitoring), T10 (runbook docs) — all `general-purpose`.
**Parallelism:**
- Phase 1 (parallel): T1 (bootstrap), T2 (worker bootstrap), T4 (ntfy), T6 (Tailscale), T7 (backup), T9 (monitoring), T10 (runbook)
- Phase 2 (serial, you): T3 (systemd), T5 (firewall), T8 (upgrade)
- Phase 3 (serial, you): full deployment test on fresh Vultr box

**Exit gate:** Fresh Vultr box → working Stronghold in <15 minutes. Workers addable in <5 minutes. Backup/restore tested. Upgrade path tested. systemd security hardening verified.

---

### Wave 11 — Integration & E2E

**Entry:** All of W0-W10 exit gates passed.
**Your role:** T2-T12 (E2E test design — security-critical). Design the tests yourself; delegate the harness implementation.
**Delegate:** T1 (E2E harness — `general-purpose`), T13-T15 (CI/coverage/release pipelines — `general-purpose`).
**Parallelism:**
- Phase 1 (serial, you): T2-T12 test design (write the test plans)
- Phase 2 (parallel): T1 (harness implementation from your plans), T13 (CI), T14 (coverage), T15 (release pipeline)
- Phase 3 (serial, you): run all E2E tests on dev box + SEV box, fix failures

**Exit gate:** All E2E tests pass on dev box (no-sev-snp) and SEV-SNP box. Load tests pass with documented throughput. CI green on main. Coverage >80% overall.

---

### Wave 12 — Hardening & Release

**Entry:** Wave 11 exit gate passed.
**Your role:** T1 (security self-audit), T2 (dependency audit), T3 (threat model validation), T8 (binary signing), T9 (measurement registry), T11 (tag + release).
**Delegate:** T4 (docs review — `general-purpose`), T5 (OpenAPI — `full-stack-developer`), T6 (operations runbook — `general-purpose`), T7 (release notes — `general-purpose`), T10 (release presentation — `ppt-expert`).
**Parallelism:**
- Phase 1 (parallel): T1 (you, security audit), T2 (you, deps), T4 (docs), T5 (OpenAPI), T6 (runbook), T7 (release notes), T10 (slides)
- Phase 2 (serial, you): T3 (threat model validation — depends on T1), T8 (signing), T9 (measurement), T11 (tag + release)
- Phase 3: T12 (7-day post-release monitoring)

**Exit gate:** v1.0.0 tagged. Binaries published and signed. SEV-SNP measurement registered. GitHub release live. 7 days post-release with no critical issues.

---

## 5. Subagent Prompt Templates

Every subagent prompt MUST include these sections. Use the templates below as starting points, adapting the task-specific details.

### 5.1 General-Purpose (GP) — for shell scripts, docs, multi-step research

```
Task ID: <W<n>-T<m>>
You are a general-purpose agent working on the Stronghold project.

CONTEXT:
- Stronghold is a self-hosted gateway for AI agents. Read /home/z/my-project/stronghold/README.md for overview.
- Read /home/z/my-project/stronghold/TASKS.md section "<W<n>-T<m>>" for your task's DoD and tests.
- Read /home/z/my-project/stronghold/worklog.md for prior context.
- Read the relevant ADRs in /home/z/my-project/stronghold/docs/adr/ if your task touches a design decision.
- Dev box: 45.63.97.103 (Rocky 10.2, Rust 1.97.1). SSH via: python3 /home/z/my-project/scripts/ssh_exec.py '<cmd>'

YOUR TASK:
<specific task description from TASKS.md>

DELIVERABLES:
- <file 1>
- <file 2>
- <test output>

CONSTRAINTS:
- Do not touch files outside your task's scope.
- Do not touch crypto/, audit/, or sessions/manager.rs.
- Run `cargo build --workspace --features no-sev-snp` before returning.
- Run `cargo test <your module>` before returning.
- Append a worklog entry to /home/z/my-project/stronghold/worklog.md with:
  ---
  Task ID: <W<n>-T<m>>
  Agent: general-purpose
  Task: <one-line summary>
  Work Log:
  - <step 1>
  - <step 2>
  Stage Summary:
  - <files changed>
  - <test results>
  - <decisions made>

RETURN TO ORCHESTRATOR:
- Summary of files changed
- Test output (pass/fail counts)
- Any decisions you made (with rationale)
- Any blockers or questions for the orchestrator
```

### 5.2 Explore (EXP) — for codebase discovery

```
Task ID: <W<n>-T<m>> (exploration)
You are an Explore agent. Your job is to map out <X> in the Stronghold codebase.

CONTEXT:
- Repo: /home/z/my-project/stronghold/
- Read TASKS.md section "<W<n>-T<m>>" for what we need to know.

YOUR TASK:
Answer these questions:
1. <question 1>
2. <question 2>
3. <question 3>

For each, provide:
- File path(s) and line numbers
- Code snippet
- Explanation

Do NOT modify any files. This is read-only exploration.

RETURN:
A structured report answering each question with file references.
```

### 5.3 Plan (PLN) — for implementation strategy

```
Task ID: <W<n>-T<m>> (planning)
You are a Plan agent. Design the implementation strategy for <task>.

CONTEXT:
- Read /home/z/my-project/stronghold/TASKS.md section "<W<n>-T<m>>".
- Read the relevant source files in /home/z/my-project/stronghold/gateway/src/.
- Read relevant ADRs.

YOUR TASK:
Produce a step-by-step implementation plan for <task>. Include:
1. Files to modify (with paths)
2. For each file: what changes, in what order
3. Dependencies between changes
4. Test strategy (which tests to write first, which to write after)
5. Risk areas (what could go wrong, what to watch for)
6. Estimated effort (hours)

Do NOT write code. This is planning only.

RETURN:
A markdown document with the plan. The orchestrator will use it to brief the implementation agent.
```

### 5.4 Full-Stack-Developer (FSD) — for HTTP routes, WebSocket, frontend

```
Task ID: <W<n>-T<m>>
You are a full-stack-developer agent working on Stronghold.

CONTEXT:
- Stronghold is a Rust (axum + tokio) gateway + vanilla JS phone PWA.
- Read /home/z/my-project/stronghold/README.md.
- Read /home/z/my-project/stronghold/TASKS.md section "<W<n>-T<m>>" for DoD and tests.
- Read /home/z/my-project/stronghold/docs/PROTOCOL.md (if touching routes).
- Read /home/z/my-project/stronghold/worklog.md for prior context.
- Dev box: 45.63.97.103. SSH: python3 /home/z/my-project/scripts/ssh_exec.py '<cmd>'

YOUR TASK:
<specific task — e.g., "Implement the /agent/order route handler">

IMPLEMENTATION REQUIREMENTS:
- Use the existing AppState struct in routes/mod.rs
- Follow the wire format in docs/PROTOCOL.md exactly
- Every handler returns Result<Json<T>, (StatusCode, String)>
- Use tracing::info! for request logging
- Parameterize all SQL (no format! in queries)

TESTS (mandatory):
- At least one happy-path test
- At least one error-case test (401, 403, 404, 408, or 410 as appropriate)
- Use tower::ServiceExt::oneshot for axum router testing
- Tests in #[cfg(test)] mod tests in the same file

CONSTRAINTS:
- Do not touch: crypto/, audit/, sessions/manager.rs, machines/scheduler.rs
- Do not add new dependencies without justification
- Run cargo build --features no-sev-snp && cargo clippy -- -D warnings
- Run cargo test gateway::routes::<your module>
- Append worklog entry (see template in §5.1)

RETURN:
- Files changed
- Test output
- Any deviations from PROTOCOL.md (with rationale)
```

### 5.5 Frontend-Styling-Expert (FST) — for CSS, UX, PWA polish

```
Task ID: <W<n>-T<m>>
You are a frontend-styling-expert working on the Stronghold phone PWA.

CONTEXT:
- The phone enrollment page is at /home/z/my-project/stronghold/phone/enroll.html
- It's a single HTML file with inline CSS and JS. No build step.
- Target browsers: mobile Safari (iOS 17+), mobile Chrome (Android latest)
- Must be installable as PWA (manifest + service worker)
- Read /home/z/my-project/stronghold/TASKS.md section "<W<n>-T<m>>" for DoD.

YOUR TASK:
<specific task — e.g., "Polish mobile UX: large tap targets, dark mode, haptic feedback">

DESIGN REQUIREMENTS:
- Dark mode (background #0a0a0a, cards #1a1a1a)
- Minimum tap target: 44pt × 44pt
- Haptic feedback on Approve/Deny (navigator.vibrate)
- VoiceOver compatible (semantic HTML, ARIA labels)
- No external CSS/JS frameworks (vanilla only)
- Lighthouse score >90 on mobile

DELIVERABLES:
- Updated phone/enroll.html
- (If needed) phone/manifest.json, phone/sw.js

CONSTRAINTS:
- Do not touch Rust code
- Do not touch the WebAuthn ceremony logic (that's the FSD's domain)
- Test in mobile Safari and Chrome (use Playwright if available)
- Append worklog entry

RETURN:
- Files changed
- Lighthouse score (if you can run it)
- Screenshots or descriptions of the UX changes
```

### 5.6 PPT-Expert (PPT) — for release presentation

```
Task ID: W12-T10
You are a ppt-expert agent. Create a 10-slide release presentation for Stronghold v1.0.0.

CONTEXT:
- Read /home/z/my-project/stronghold/README.md
- Read /home/z/my-project/stronghold/docs/THREAT_MODEL.md
- Read /home/z/my-project/stronghold/CHANGELOG.md
- Read /home/z/my-project/stronghold/docs/adr/ for design rationale

YOUR TASK:
Create a 10-slide standalone HTML presentation covering:
1. Title slide (Stronghold v1.0.0)
2. The problem (AI agents need safe execution environments)
3. Architecture overview (control plane + k3s workers)
4. Security model (PQ crypto + SEV-SNP + WebAuthn)
5. Agent protocol (ORDER/RESUME/RELEASE/EXTEND)
6. Image catalog (rocky-base + 7 derived)
7. Demo: full session lifecycle
8. Deployment patterns (single-box, multi-box, community)
9. What's next (v1.1 roadmap)
10. Call to action (GitHub link, contribute)

DELIVERABLE:
- /home/z/my-project/stronghold/docs/releases/v1.0.0-slides.html
- Standalone HTML, no external dependencies
- Professional design (dark theme matching the PWA)

RETURN:
- File path
- Slide count
- Any notes on design decisions
```

---

## 6. Parallelism Strategy

### 6.1 When to parallelize

Parallelize when ALL of these are true:
- The wave's entry gate has passed
- The tasks have no dependencies on each other
- The tasks touch different files (no merge conflicts)
- Each task is independently testable

### 6.2 When NOT to parallelize

Do NOT parallelize when ANY of these are true:
- A task touches `crypto/`, `audit/`, `sessions/manager.rs`, `machines/scheduler.rs`, or `tee/` (security-critical — orchestrator only)
- Tasks share a file (sequential edits needed)
- A task's tests depend on another task's output
- The wave is on the critical path (W0, W1, W11, W12)

### 6.3 How to parallelize

Use a **single message with multiple `Task` tool calls**. Example for Wave 2 Phase 2:

```python
# In one assistant message, spawn 6 subagents in parallel:
Task(subagent_type="full-stack-developer", description="W2-T3 tenant registry", prompt="<template>")
Task(subagent_type="full-stack-developer", description="W2-T4 quotas", prompt="<template>")
Task(subagent_type="full-stack-developer", description="W2-T5 agent tokens", prompt="<template>")
Task(subagent_type="full-stack-developer", description="W2-T6 phone tokens", prompt="<template>")
Task(subagent_type="full-stack-developer", description="W2-T7 WebAuthn enroll", prompt="<template>")
Task(subagent_type="general-purpose", description="W2-T9 backup/restore", prompt="<template>")
```

All 6 subagents run concurrently. You wait for all to return before reviewing.

### 6.4 Max concurrency

- **Hard limit: 6 concurrent subagents.** Beyond this, review quality drops and merge conflicts increase.
- If a wave has more than 6 parallelizable tasks, batch them: 6 in parallel, review, then next 6.
- Within a batch, ensure no two subagents touch the same file.

### 6.5 Cross-wave parallelism

After Wave 4 (Routes & PTY) is done, Waves 5, 6, 7, 8, 9, 10 can ALL run in parallel. This is the big speedup.

**Practical approach:** Don't actually run 6 waves in parallel — too much context to hold. Instead:
1. Spawn subagents for the delegated tasks across W5, W6, W8, W9, W10 in one batch (up to 6 concurrent).
2. While they run, you do W7 (SEV-SNP) yourself — it's serial and security-critical.
3. As subagents return, review and integrate.
4. Spawn the next batch.

This gives you ~3x speedup over pure serial execution.

---

## 7. Quality Gates

Each wave has an exit gate. The gate MUST pass before the next wave starts. No exceptions.

### 7.1 Per-task gate (before marking [x])

- [ ] Code compiles (`cargo build --features no-sev-snp`)
- [ ] No new clippy warnings (`cargo clippy -- -D warnings`)
- [ ] Unit tests pass (`cargo test <module>`)
- [ ] DoD from TASKS.md is met (every bullet checked)
- [ ] Worklog entry appended
- [ ] No `todo!()` in new code (unless explicitly deferred with a documented reason)

### 7.2 Per-wave gate (before starting next wave)

- [ ] All tasks in wave marked [x]
- [ ] `cargo build --workspace --features no-sev-snp` clean
- [ ] `cargo build --workspace --features sev-snp` clean (compiles, even if can't run on dev box)
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo fmt --all -- --check` clean
- [ ] Git commit pushed to GitHub
- [ ] Dev box synced (`git pull` on 45.63.97.103, build passes)
- [ ] Wave-specific DoD from TASKS.md met

### 7.3 Project-level gate (before v1.0.0 tag)

See "Definition of Done — Project Level" in TASKS.md. All bullets must be checked.

### 7.4 What to do when a gate fails

1. **Do not proceed to the next wave.**
2. Identify the failing task.
3. If it's your code: fix it.
4. If it's a subagent's code: read the worklog, understand what they did, either fix yourself or re-brief the subagent with specific feedback.
5. Re-run the gate.
6. Only proceed when green.

---

## 8. Worklog Protocol

The worklog is the shared state across all agents. Treat it as sacred.

### 8.1 Location

`/home/z/my-project/stronghold/worklog.md`

### 8.2 Format

```markdown
# Stronghold Worklog

---
Task ID: W0-T1
Agent: orchestrator
Task: Fix rustls 0.23 pqc-kyber feature removal

Work Log:
- Read Cargo.toml line 31: `rustls = { version = "0.23", features = ["ring", "pqc-kyber"] }`
- Checked rustls 0.23 docs: `pqc-kyber` feature was removed in 0.23.10
- Replaced with `rustls = { version = "0.23", features = ["ring", "aws_lc_rs"] }` + `rustls-post-quantum = "0.2"`
- Updated gateway/Cargo.toml to add `rustls-post-quantum = { workspace = true }`
- Ran `cargo build --features no-sev-snp`: rustls resolves now
- Remaining errors: 17 (down from 18)

Stage Summary:
- Cargo.toml, gateway/Cargo.toml modified
- rustls PQ hybrid now via aws_lc_rs + rustls-post-quantum crate
- Next: W0-T2 (audit mod file-not-found)
```

### 8.3 Rules

- Every task gets an entry. No exceptions.
- Append only. Never edit prior entries.
- Use `---` as separator between entries.
- The orchestrator reads the worklog before starting any task.
- Subagents read the worklog before starting their task (mentioned in their prompt).
- After completing a task, the agent (orchestrator or subagent) appends their entry.

### 8.4 Worklog initialization

If `worklog.md` doesn't exist, create it with:
```markdown
# Stronghold Worklog

Started: <ISO date>
Repo: github.com/pkhairkh/stronghold
Dev box: 45.63.97.103

---
Task ID: W0-T0
Agent: orchestrator
Task: Initialize worklog

Work Log:
- Created worklog.md
- Read TASKS.md
- Verified dev box accessible

Stage Summary:
- Ready to start Wave 0
```

---

## 9. Failure Modes & Recovery

### 9.1 Subagent returns broken code

**Symptom:** Subagent claims done, but `cargo build` fails or tests fail.

**Recovery:**
1. Read the subagent's worklog entry and the files they changed.
2. Identify the specific failure (compile error, test failure).
3. If it's a small fix (1-5 lines): fix it yourself, note in worklog.
4. If it's a fundamental misunderstanding: re-brief the subagent with explicit correction.
5. Re-verify.

### 9.2 Subagent touches files outside their scope

**Symptom:** Subagent modified `crypto/` or `audit/` when they shouldn't have.

**Recovery:**
1. `git diff` to see what they changed.
2. Revert the out-of-scope changes: `git checkout -- <file>`.
3. Re-brief the subagent with a stronger constraint: "Do NOT modify <file>. If you think you need to, stop and ask."
4. Re-verify.

### 9.3 Dev box becomes unreachable

**Symptom:** `ssh_exec.py` times out or connection refused.

**Recovery:**
1. Try again after 30s (transient network).
2. Try pinging: `ping 45.63.97.103`.
3. If still down: Vultr box may have crashed. Use Vultr console to reboot.
4. If reboot fails: provision a new box, update `ssh_exec.py` with new IP, re-clone repo.

### 9.4 Build breaks on dev box but not locally

**Symptom:** Local build clean, dev box build fails.

**Recovery:**
1. Check Rust version: `python3 ssh_exec.py 'rustc --version'`. Should be 1.97.1+.
2. Check dependencies: `python3 ssh_exec.py 'cd /root/stronghold && cargo tree | head -50'`.
3. Pull latest on dev box: `python3 ssh_exec.py 'cd /root/stronghold && git pull && cargo build --features no-sev-snp'`.
4. If still broken: diff `Cargo.lock` between local and dev box.

### 9.5 Tests pass locally but fail on dev box

**Symptom:** `cargo test` green locally, red on dev box.

**Recovery:**
1. Check for hardcoded paths (tests should use relative paths or env vars).
2. Check for timing-dependent tests (increase timeout).
3. Check for port conflicts (`lsof -i :8443`).
4. Run the failing test with `--nocapture` on dev box for more output.

### 9.6 GitHub push fails

**Symptom:** `git push` returns auth error.

**Recovery:**
1. The PAT may have expired or been revoked. Get a new one from the user.
2. Update the push command with the new PAT.
3. Push.
4. Reset remote URL to public form.

### 9.7 SEV-SNP box fails to attest

**Symptom:** Gateway on SEV box refuses to start, "attestation failed".

**Recovery:**
1. Check `/dev/sev` exists: `ls -la /dev/sev`.
2. Check kernel module: `lsmod | grep sev`.
3. Check dmesg: `dmesg | grep -i sev`.
4. If the Vultr plan doesn't actually support SEV-SNP: reprovision with the correct plan.
5. If the binary measurement changed (expected after upgrade): run key rotation ceremony.

### 9.8 Audit log verification fails

**Symptom:** `stronghold audit verify` reports hash chain break or signature failure.

**Recovery:**
1. Identify the specific entry that fails.
2. Check if it's a real tamper or a bug in the verifier.
3. If real tamper: this is a security incident. Stop the gateway. Investigate. Document in `docs/SECURITY_INCIDENTS.md`.
4. If verifier bug: fix the verifier, add a regression test.

---

## 10. Orchestrator's Daily Rhythm

When you (the orchestrator) sit down to work on Stronghold, follow this rhythm:

### 10.1 Start of session

1. Read `worklog.md` — what was the last completed task?
2. Read `TASKS.md` — what's the next task/wave?
3. Check dev box: `python3 ssh_exec.py 'cd /root/stronghold && git log --oneline -3 && cargo build --features no-sev-snp 2>&1 | tail -3'`
4. Check GitHub: is main ahead of dev box? If so, pull.
5. Decide: what will I accomplish today? (1-5 tasks, depending on complexity.)

### 10.2 During work

1. For each task: read its TASKS.md entry, read the relevant source files, implement or delegate.
2. After each task: update worklog, mark [x] in local TASKS.md, commit.
3. After every 3-5 tasks: push to GitHub, sync dev box.

### 10.3 End of session

1. Push any uncommitted work.
2. Sync dev box.
3. Update TASKS.md with any new tasks discovered.
4. Write a "session summary" worklog entry:
```markdown
---
Task ID: SESSION-<date>
Agent: orchestrator
Task: Session summary

Work Log:
- Completed: W<n>-T<m>, W<n>-T<m+1>, ...
- In progress: W<n>-T<m+2>
- Blocked on: <nothing, or description>

Stage Summary:
- <waves completed>
- <waves remaining>
- <next session's first task>
```

---

## 11. Quick Reference: Subagent Type by Task

| Task Pattern | Subagent Type |
|---|---|
| Shell scripts (bootstrap, upgrade) | `general-purpose` |
| Documentation | `general-purpose` |
| CI/CD pipelines | `general-purpose` |
| Vultr API integration | `general-purpose` |
| E2E test harness | `general-purpose` |
| Codebase exploration | `Explore` |
| Implementation strategy | `Plan` |
| HTTP route handlers | `full-stack-developer` |
| WebSocket / PTY code | `full-stack-developer` (or orchestrator if security-critical) |
| Frontend HTML/JS | `full-stack-developer` |
| CSS / UX polish | `frontend-styling-expert` |
| Release slides | `ppt-expert` |
| Crypto / signatures / KEM | **Orchestrator (never delegate)** |
| Audit log | **Orchestrator (never delegate)** |
| Auth / tokens / WebAuthn verify | **Orchestrator (never delegate)** |
| Session state machine | **Orchestrator (never delegate)** |
| SEV-SNP / TEE | **Orchestrator (never delegate)** |
| Database schema / migrations | **Orchestrator (never delegate)** |
| SQL injection audit | **Orchestrator (never delegate)** |
| systemd security hardening | **Orchestrator (never delegate)** |
| Firewall rules | **Orchestrator (never delegate)** |

---

## 12. Final Checklist Before You Start

Before executing your first task, verify:

- [ ] You have read this document end-to-end.
- [ ] You have read `TASKS.md` end-to-end.
- [ ] You have read `README.md` and understand what Stronghold is.
- [ ] You have read all 10 ADRs in `docs/adr/`.
- [ ] You have read `docs/THREAT_MODEL.md`.
- [ ] You have SSH access to the dev box (run `python3 /home/z/my-project/scripts/ssh_exec.py 'uname -a'`).
- [ ] You have push access to GitHub (test with a dry-run).
- [ ] `worklog.md` exists (if not, initialize per §8.4).
- [ ] The dev box has the latest `main` (`git pull` on dev box).
- [ ] The dev box builds (even with 18 errors — that's Wave 0's job to fix).

If any of these fail, fix them before starting Wave 0.

---

## 13. Begin

Your first task is **W0-T1**: Fix the `rustls` 0.23 `pqc-kyber` feature removal.

Read `TASKS.md` Wave 0 section. Read `gateway/Cargo.toml`. Fix it. Build. Verify. Commit. Push. Update worklog. Mark `[x]`. Move to W0-T2.

Go.

---

*Document version: 1.0*
*Last updated: 2026-07-29*
*Maintained by: Orchestrating agent (you)*
