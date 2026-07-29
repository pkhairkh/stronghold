# Stronghold — Alpha-to-Beta Execution Prompt

> Surgical, subagent-based prompt to close the 18 known gaps and move Stronghold from alpha to beta. Each task is scoped to touch 1-3 files max, has a concrete DoD with tests, and is completable in isolation.

---

## 0. Orchestrator Instructions

You are the orchestrating agent. Your job:

1. Read `worklog.md` for prior context.
2. Execute waves in order: A (security-critical) → B (feature completion) → C (hardening).
3. Within each wave, tasks are independent — spawn subagents in parallel.
4. **One task per subagent.** Each subagent gets exactly one task ID, one file scope, one DoD.
5. After each wave: sync dev box, run `cargo build && cargo clippy -- -D warnings && cargo test`, fix any breakage, commit, push.
6. Never delegate: crypto signature verification, TLS wiring, auth checks. Those are security-critical — do them yourself.

### Dev box access
```bash
python3 /home/z/my-project/scripts/ssh_exec.py '<command>'
python3 /home/z/my-project/scripts/ssh_exec.py --file <local_script.sh>
python3 /home/z/my-project/scripts/ssh_exec.py --upload <local> <remote>
```

### Git workflow
```bash
# After each task:
cd /home/z/my-project/stronghold
git add -A && git commit -m "<task-id>: <summary>"
git push origin main
git remote set-url origin https://github.com/pkhairkh/stronghold.git
```

### Quality gate (after each wave)
```bash
# On dev box:
cd /root/stronghold && git fetch origin && git reset --hard origin/main
cargo build --workspace --features no-sev-snp
cargo clippy --workspace --features no-sev-snp -- -D warnings
cargo test --workspace --features no-sev-snp
```
All three must pass before proceeding to the next wave.

---

## Wave A — Security-Critical (serial, orchestrator-only)

These 3 tasks are security-critical. **Do NOT delegate.** Do them yourself, one at a time, verify each before moving on.

### A1: Wire TLS into axum::serve()

**File:** `gateway/src/main.rs` (only)
**Current state:** Line ~138 does `let _tls_config = crypto::tls::build_client_config();` then serves plain HTTP via `axum::serve(listener, ...)`. The TLS config is computed and discarded.
**Fix:** 
- On startup, check if `/var/lib/stronghold/keys/tls.crt` and `tls.key` exist. If not, call `crypto::tls::generate_self_signed_cert_files("/var/lib/stronghold/keys/", "localhost")?` to auto-generate.
- Call `crypto::tls::build_server_config_from_files("/var/lib/stronghold/keys/")?` to load the cert+key.
- Replace `axum::serve(listener, app.into_make_service())` with `axum_server::bind_rustls(addr, tls_config).serve(app.into_make_service())`.
- Add `axum-server = { version = "0.7", features = ["tls-rustls"] }` to `gateway/Cargo.toml`.
- Keep the `--dev` path: if `STRONGHOLD_DEV` env var is set, serve plain HTTP (skip TLS) for local testing.
**DoD:** Gateway starts with HTTPS on port 8443. `curl -k https://localhost:8443/agent/health` returns 200. In dev mode, `curl http://localhost:8443/agent/health` returns 200.
**Test:** Add test in `main.rs` or `tls.rs` that verifies `build_server_config_from_files()` succeeds after `generate_self_signed_cert_files()`.
**Context budget:** ~50 lines changed in main.rs, ~1 line in Cargo.toml.

### A2: Implement WebAuthn signature verification

**File:** `gateway/src/crypto/webauthn.rs` (only)
**Current state:** `verify_assertion()` at line ~150 does metadata checks (challenge, origin, UV flag, RP ID hash) then logs `"WebAuthn signature verification not yet implemented — accepting based on metadata only"` and returns `Ok(true)`. The actual signature bytes in `assertion.signature` are never checked.
**Fix:**
- The `WebAuthnAssertion` struct (in `routes/phone.rs`) has fields: `credential_id`, `authenticator_data`, `client_data_json`, `signature` — all base64url-encoded strings.
- To verify the signature, you need the credential's **public key** from the database. The `credentials` table has a `public_key` column (stored as a string — currently the raw COSE public key from enrollment).
- The signature is over `authenticator_data || SHA-256(client_data_json)`. This is the standard WebAuthn assertion signature.
- The public key is in COSE format (CBOR). Use the `webauthn-rs` crate's `WebauthnCore` or manually parse the COSE key and verify with `p256` (ECDSA) or `ed25519-dalek` (Ed25519).
- Simplest approach: use `webauthn_rs_proto::PublicKeyCredential` + `webauthn_rs::WebauthnCore::verify_authentication_response()`. You'll need to construct the `AuthenticationResult` from the assertion fields.
- If the COSE key format is ambiguous, add a `key_type` column to the `credentials` table (migration 002) and store the algorithm (-7 for ES256, -8 for Ed25519).
- **If full COSE parsing is too complex for this task**, at minimum: decode the signature, decode the authenticator_data, compute `authenticator_data || SHA256(client_data_json)`, and verify the signature using the stored public key bytes with the `p256` crate (ECDSA P-256, the most common authenticator type).
**DoD:** `verify_assertion()` returns `Ok(false)` for a tampered signature. `verify_assertion()` returns `Ok(true)` only when the signature verifies against the stored credential public key. Remove the "not yet implemented" warning log.
**Test:** Generate an ECDSA P-256 keypair, store the public key in the DB, sign a challenge with the private key, call `verify_assertion()` — must return true. Tamper the signature — must return false.
**Context budget:** ~100-150 lines in webauthn.rs. May need `p256 = "0.13"` or `webauthn-rs` features in Cargo.toml.

### A3: Add PTY connect_token verification

**File:** `gateway/src/routes/pty.rs` (only)
**Current state:** `handle_pty_ws()` accepts any WebSocket upgrade without checking authentication. Anyone who knows a `machine_id` can attach.
**Fix:**
- The `OrderResponse` returned by `/agent/order` includes a `connect_token` field. This token is what should be checked.
- In `handle_pty_ws`, extract the `connect_token` from either:
  - A query parameter: `ws://gateway/agent/{machine_id}/pty?token={connect_token}`
  - Or the `Sec-WebSocket-Protocol` header (common pattern for browser WS auth)
- Verify the token: look up the `machine_id` in the `machines` table, check the machine is `active` and not expired. The connect_token is ephemeral (generated at ORDER time) — store it in the `machines` table (add a `connect_token_hash` column via migration 003) and compare SHA-256 hashes.
- If verification fails, reject the WebSocket upgrade with HTTP 401.
- Also verify the `machine_id` in the URL belongs to the tenant that owns the token.
**DoD:** WebSocket connection without a valid `connect_token` is rejected (401). WebSocket connection with a valid token succeeds. WebSocket connection with a token for a different `machine_id` is rejected.
**Test:** Unit test the token verification function. Integration test: create a machine, try connecting without token (reject), with wrong token (reject), with correct token (accept).
**Context budget:** ~60 lines in pty.rs. May need a migration for `connect_token_hash` column.

---

## Wave B — Feature Completion (parallel, subagent-delegated)

After Wave A passes the quality gate, spawn these subagents in parallel. Each is independent.

### B1: Wire E2E encryption into production push paths

**Subagent type:** `general-purpose`
**Task ID:** B1
**Files:** `gateway/src/push/ntfy.rs` (only)
**Current state:** `push_approval_request()`, `push_extend_request()`, `push_anomaly()`, `push_revoked()`, `push_daily_digest()` all call `send_notification()` which sends plaintext. The `send_encrypted_notification_to()` function exists but is only used in tests.
**Fix:** Change all 5 production push functions to call `send_encrypted_notification_to()` instead of `send_notification()`. Each function needs the phone's X25519 + ML-KEM-768 public keys (from the `phone_push_keys` table). Add a helper `get_phone_push_keys(db, tenant_id) -> (Vec<u8>, Vec<u8>)` that queries the DB. If no keys are enrolled, fall back to plaintext with a warning log.
**DoD:** All push notifications are E2E-encrypted when phone keys are enrolled. Tests verify the ntfy body is base64 ciphertext (not plaintext JSON).
**Context budget:** ~80 lines changed in ntfy.rs.

### B2: Wire anomaly scanner into PTY proxy

**Subagent type:** `general-purpose`
**Task ID:** B2
**Files:** `gateway/src/routes/pty.rs` (only)
**Current state:** `pty_proxy()` has an `audit_handle` that does nothing (`// TODO: stream bytes to audit log`). The `AnomalyScanner` in `anomaly/mod.rs` is never instantiated.
**Fix:** In `pty_proxy()`, after the container sends output bytes, run them through `AnomalyScanner::scan()`. If a pattern matches, call `push::ntfy::push_anomaly(tenant_id, machine_id, message)`. Also log every command to the audit log via `audit::log::entry()`. Instantiate the scanner with `AnomalyScanner::defaults()` (or load from config file if present).
**DoD:** Running `curl evil.com` in the PTY triggers an anomaly push. Running `ls -la` does not. Audit log contains entries for PTY activity.
**Context budget:** ~40 lines added to pty.rs.

### B3: Replace phone SSE heartbeat with real pending-approvals stream

**Subagent type:** `general-purpose`
**Task ID:** B3
**Files:** `gateway/src/sessions/manager.rs` (only — the `pending_approval_stream` function)
**Current state:** `pending_approval_stream()` emits `"heartbeat"` every 30s in an infinite loop. The phone never receives actual approval requests.
**Fix:** Replace the heartbeat loop with a real poll: query the `pending_sessions` table every 500ms for sessions with `status = 'pending'` and `tenant_id = ?`. When a new pending session is found, yield an SSE event with the session details (JSON: `session_id`, `image`, `ttl_secs`, `reason`, `created_at`). Keep the 30s heartbeat as a keepalive between events. Track the last-seen session ID to avoid re-yielding old sessions.
**DoD:** When an agent calls ORDER, the phone SSE stream receives an `approval_request` event within 1 second. No duplicate events for the same session.
**Context budget:** ~50 lines changed in manager.rs.

### B4: Implement audit streaming to PTY WebSocket

**Subagent type:** `general-purpose`
**Task ID:** B4
**Files:** `gateway/src/routes/pty.rs` (only — the `audit_stream` function)
**Current state:** `audit_stream()` sends `"Audit stream not yet implemented"` and returns immediately.
**Fix:** Replace with a real stream: query the `audit_entries` table for entries with `machine_id = ?`, ordered by `seq`. Stream them as JSON objects over the WebSocket. Use long-polling (check every 500ms for new entries). Track the last-seen `seq` to avoid re-sending. Send a keepalive every 30s.
**DoD:** Opening the audit WebSocket for a machine with existing audit entries streams them as JSON. New entries appear within 1 second.
**Context budget:** ~40 lines changed in pty.rs.

### B5: Fix --dev flag bug

**Subagent type:** `general-purpose`
**Task ID:** B5
**Files:** `gateway/src/main.rs` (only)
**Current state:** `main.rs` has a `--dev` clap flag that sets `cli.dev: bool`. But `serve()` checks `std::env::var("STRONGHOLD_DEV")` — the env var, not the struct field. So `stronghold-gateway serve --dev` on a non-SEV box still fails at `tee::verify_sev_snp_available()?`.
**Fix:** Pass the `dev` flag through to `serve()`. Change `serve(bind_addr)` to `serve(bind_addr, dev)`. In `serve()`, check the `dev` parameter instead of (or in addition to) the env var. If `dev` is true, skip `tee::verify_sev_snp_available()` and log a warning.
**DoD:** `stronghold-gateway serve --dev` starts successfully on the dev box (no `/dev/sev`) without setting any env vars.
**Context budget:** ~10 lines changed in main.rs.

### B6: Implement real worker add/list via k3s API

**Subagent type:** `general-purpose`
**Task ID:** B6
**Files:** `gateway/src/machines/worker.rs` (only)
**Current state:** `add()` logs and returns `Ok(())`. `list()` returns `Ok(vec![])`. `health_check()` returns `Ok(true)`.
**Fix:** 
- `list()`: use `kube::Api::<Node>::list()` to list all k3s nodes. Return `Vec<Worker>` with host (node name), cpu_total, memory_gb_total, sev_snp (false for now), status.
- `add()`: this can't actually SSH to a remote box from Rust easily. Instead, document that workers are added via `setup/worker-bootstrap.sh` and this function just verifies the node appears in the k3s cluster after bootstrap. Or: call the Vultr API to create a VPS, then wait for it to join k3s.
- `health_check()`: query `kube::Api::<Node>::get(host)` and check the `Ready` condition.
**DoD:** `list()` returns real k3s nodes. `health_check()` returns false for a non-existent node, true for a healthy one.
**Context budget:** ~60 lines in worker.rs.

### B7: Implement image build via podman

**Subagent type:** `general-purpose`
**Task ID:** B7
**Files:** `gateway/src/images/builder.rs` (only)
**Current state:** `build()` generates the Containerfile to a temp dir, logs it, returns a fake digest. The `// TODO: call podman or docker build` comment is at line ~130.
**Fix:** After writing the Containerfile, invoke `std::process::Command::new("podman").args(["build", "-t", tag, "-f", containerfile_path, "."]).output()`. Check the exit code. If successful, run `podman inspect --format '{{.Digest}}' <tag>` to get the image digest. Return the real digest. If podman is not installed, return an error with a helpful message.
**DoD:** `build()` produces a real OCI image in the local podman store. The returned digest matches `podman inspect`.
**Context budget:** ~30 lines in builder.rs.

### B8: Implement audit verify CLI signature checks

**Subagent type:** `general-purpose`
**Task ID:** B8
**Files:** `gateway/src/audit/verify.rs` (only)
**Current state:** `verify_tenant()` walks the hash chain but has `// TODO: verify Ed25519 signature / TODO: verify ML-DSA-65 signature / TODO: verify SEV-SNP attestation report`. Only the hash chain is checked.
**Fix:** After the hash chain check, load the audit keys from `/var/lib/stronghold/keys/` (via `AuditKeys::load()`). For each entry, decode the `sig_ed25519` and `sig_mldsa65` fields from base64, reconstruct the signed message (`ts|tenant_id|machine_id|event|payload|prev_hash`), and verify with `AuditKeys::verify()`. Report any signature failures.
**DoD:** `verify_tenant()` reports "Ed25519 signatures: OK" or "FAILED at seq N". Tampered entries are detected.
**Context budget:** ~40 lines in verify.rs.

### B9: Add Prometheus /metrics endpoint

**Subagent type:** `general-purpose`
**Task ID:** B9
**Files:** `gateway/src/routes/mod.rs` (add route), new file `gateway/src/routes/metrics.rs`
**Current state:** No `/metrics` route exists.
**Fix:** Add a `GET /metrics` endpoint that returns Prometheus-format metrics. Use the `prometheus` crate (or just format strings manually). Metrics to expose:
- `stronghold_sessions_active` (gauge)
- `stronghold_approvals_pending` (gauge)
- `stronghold_audit_entries_total` (counter)
- `stronghold_machines_total` (gauge by status)
Query the DB for counts. Register the route in `build_router()`.
**DoD:** `curl http://localhost:8443/metrics` returns Prometheus-format text with the above metrics.
**Context budget:** ~60 lines in new metrics.rs + 2 lines in mod.rs.

---

## Wave C — Hardening (parallel after Wave B)

### C1: Implement quorum enforcement in PTY proxy

**Subagent type:** `general-purpose`
**Task ID:** C1
**Files:** `gateway/src/routes/pty.rs` (only)
**Current state:** `sessions/scopes.rs` has `matches_deceptive_pattern()` but the PTY proxy never calls it.
**Fix:** In `pty_proxy()`, before forwarding agent input to the container, check if the input matches a destructive pattern. If it does:
1. Don't execute the command.
2. Create a quorum approval request in the DB (new table `quorum_requests` or reuse `pending_sessions` with `is_quorum=1`).
3. Push all enrolled credentials via ntfy.
4. Block until N credentials approve (poll DB).
5. On approval, execute the command. On denial/timeout, send "Command denied by quorum" to the agent.
**DoD:** Running `rm -rf /tmp/test` in the PTY triggers a quorum push. Command doesn't execute until 2 credentials approve. Command executes after approval.
**Context budget:** ~80 lines in pty.rs.

### C2: Add rate limiting on /agent/* endpoints

**Subagent type:** `general-purpose`
**Task ID:** C2
**Files:** `gateway/src/routes/mod.rs` (only)
**Current state:** No rate limiting.
**Fix:** Add `tower::limit::ConcurrencyLimit` or `tower_governor` middleware to the `/agent/*` routes. Limit to 10 concurrent ORDERs per agent token. Return 429 when exceeded.
**DoD:** 11th concurrent ORDER from the same token returns 429.
**Context budget:** ~20 lines in mod.rs.

### C3: Add structured request tracing

**Subagent type:** `general-purpose`
**Task ID:** C3
**Files:** `gateway/src/routes/mod.rs` (only)
**Current state:** No request tracing middleware.
**Fix:** Add `tower_http::trace::TraceLayer` to the router. Log every request with method, path, status, latency. Use `tracing::info!` with structured fields. Add a request ID header.
**DoD:** Every HTTP request produces a structured log line with method, path, status, latency_ms, request_id.
**Context budget:** ~15 lines in mod.rs.

### C4: Load test — 100 concurrent sessions

**Subagent type:** `general-purpose`
**Task ID:** C4
**Files:** new file `gateway/tests/load_test.rs`
**Fix:** Write a test that creates 100 tenants, mints 100 agent tokens, creates 100 pending sessions concurrently, approves them all, and verifies 100 audit entries. Measure throughput. Assert all complete in < 30 seconds.
**DoD:** Test passes. Documented throughput in the test output.
**Context budget:** ~80 lines in new file.

### C5: Security self-audit

**Subagent type:** `general-purpose`
**Task ID:** C5
**Files:** new file `docs/SECURITY_AUDIT.md`
**Fix:** Review every `unsafe` block, every `unwrap()`, every `expect()`, every place user input flows into SQL or shell commands. Document findings. Fix any critical issues found.
**DoD:** `docs/SECURITY_AUDIT.md` exists with findings. All critical issues fixed.
**Context budget:** ~100 lines in new doc + fixes.

### C6: Dependency audit

**Subagent type:** `general-purpose`
**Task ID:** C6
**Files:** `Cargo.lock` (review only)
**Fix:** Run `cargo audit` and `cargo deny check`. Fix any RUSTSEC advisories. Document the audit results.
**DoD:** `cargo audit` reports 0 advisories. `cargo deny check` passes.
**Context budget:** ~0 lines (just running tools + fixing deps).

---

## Execution Order

```
Wave A (serial, orchestrator):
  A1 → verify → A2 → verify → A3 → verify → quality gate

Wave B (parallel, 6 subagents):
  B1, B2, B3, B4, B5, B6 → review all → quality gate
  Then: B7, B8, B9 (second batch) → quality gate

Wave C (parallel, 4 subagents):
  C1, C2, C3, C4 → review all → quality gate
  Then: C5, C6 → quality gate

Final: Tag v0.10.0-beta
```

## Subagent Prompt Template

For each delegated task, use this template:

```
Task ID: <ID>

You are fixing ONE specific gap in the Stronghold project.

FILE SCOPE: You may ONLY modify these files:
- <file 1>
- <file 2 if needed>
Do NOT touch any other files.

CURRENT STATE: <what the code does now, with line numbers>

FIX: <precise description of what to change>

CONSTRAINTS:
- Do NOT touch files outside the scope listed above.
- Do NOT change function signatures unless explicitly told to.
- Run: cd /root/stronghold && cargo build --workspace --features no-sev-snp
- Run: cd /root/stronghold && cargo clippy --workspace --features no-sev-snp -- -D warnings
- Run: cd /root/stronghold && cargo test --workspace --features no-sev-snp
- All three must pass before you return.
- Commit and push:
  cd /root/stronghold && git add -A && git commit -m "<ID>: <summary>"
  git push origin main
  git remote set-url origin https://github.com/pkhairkh/stronghold.git

DOD: <what "done" looks like>
TESTS: <what tests to write>

Return: files changed, test count, any issues.
```

## After All Waves

1. Update `CHANGELOG.md` — change version to `[0.10.0-beta]`, move items from "Known Open Gaps" to "Added" or "Fixed".
2. Update `README.md` — change badge from `alpha` to `beta`. Remove the "DO NOT DEPLOY IN PRODUCTION" warning (replace with "Beta — not recommended for production without further testing").
3. Update `SECURITY.md` — update status indicators (most should be ✅ now).
4. Update `docs/releases/` — create `v0.10.0-beta.md` with release notes.
5. Tag `v0.10.0-beta`.
6. Update `TASKS.md` — check off completed items in the Definition of Done.
7. Append final worklog entry.
