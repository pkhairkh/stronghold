# Stronghold — Beta to v1.0 RC Execution Prompt

> 6 waves, 42 surgical tasks. Each touches 1-3 files, has a concrete DoD, and is designed to fit in a single subagent context window. Orchestrator commits after each task, pushes after each wave, and may only return when all wave DoDs pass.

---

## 0. Orchestrator Protocol

### Execution loop
```
for each wave D..I:
    1. READ worklog.md + this prompt for the wave
    2. PLAN: decide orchestrator-only vs delegated tasks
    3. EXECUTE: spawn subagents (max 4 parallel), do own tasks serially
    4. REVIEW: read every changed file, run build+clippy+test
    5. COMMIT: one commit per task (format: "D1: <summary>")
    6. PUSH: push after all tasks in the wave pass
    7. GATE: run wave DoD — if fail, fix or re-brief subagent, re-gate
    8. NEXT WAVE
```

### Hard rules
- One task per subagent. Each subagent gets: task ID, file scope (1-3 files), current state, fix, DoD, test requirements.
- Orchestrator does NOT delegate: crypto, auth, security policy, k8s networking, DB schema changes.
- After each task: `cargo build && cargo clippy -- -D warnings && cargo test` on dev box.
- After each wave: push + sync dev box + run wave DoD.
- Orchestrator may only return when ALL wave DoDs pass.

### Dev box access
```bash
python3 /home/z/my-project/scripts/ssh_exec.py '<command>'
python3 /home/z/my-project/scripts/ssh_exec.py --file <local_script.sh>
python3 /home/z/my-project/scripts/ssh_exec.py --upload <local> <remote>
```

### Git workflow
```bash
cd /home/z/my-project/stronghold
git add <specific-files> && git commit -m "<task-id>: <summary>"
git push origin main
```

### Quality gate (after each task)
```bash
cd /root/stronghold && git fetch origin && git reset --hard origin/main
cargo build --workspace --features no-sev-snp
cargo clippy --workspace --features no-sev-snp -- -D warnings
cargo test --workspace --features no-sev-snp
```

---

## Wave D — Session Lifecycle (6 tasks)

**Goal:** Make sessions usable for real work — persistent volumes, file transfer, terminal resize, timeout enforcement, graceful shutdown.

**Entry condition:** v0.10.1-beta tag, 283 tests pass.

### D1: Persistent volumes for pods (orchestrator-only — k8s schema change)
**Files:** `gateway/src/machines/scheduler.rs`
**Current:** Pod spec uses `emptyDir` volumes. Work is lost when pod dies.
**Fix:** Replace `emptyDir` with `PersistentVolumeClaim` for `work` and `cache` volumes. Use `StorageClass: local-path` (k3s default). PVC name: `work-<machine_id>`, size: 10Gi (configurable). On RESUME, the scheduler should look up the existing PVC by label and reattach.
**DoD:** Create a pod, write a file to `/home/dev/work/`, kill the pod, create a new pod with the same machine_id, the file is still there.
**Test:** Integration test that creates a PVC, verifies it's `Bound`, deletes the pod, verifies PVC survives.

### D2: Session timeout reaper (subagent)
**Files:** `gateway/src/main.rs` (add reaper task), `gateway/src/sessions/manager.rs` (add `expire_overdue_sessions` function)
**Current:** `expires_at` column exists but nothing kills pods when TTL expires.
**Fix:** Add a background tokio task in `serve()` that runs every 60 seconds, queries `SELECT id, tenant_id FROM machines WHERE status = 'active' AND expires_at < datetime('now')`, calls `kill_pod()` for each, updates status to `expired`, writes audit entry.
**DoD:** Create a session with TTL=2s, wait 5s, verify pod is killed and status is `expired` in DB.
**Test:** Unit test `expire_overdue_sessions` with a mock DB where `expires_at` is in the past.

### D3: Terminal resize (subagent)
**Files:** `gateway/src/routes/pty.rs` (only)
**Current:** PTY stays at 80x24. No resize handling.
**Fix:** Add a WebSocket message type for resize: `{ "type": "resize", "cols": 120, "rows": 40 }`. When received, call `kube exec` resize endpoint (or send SIGWINCH via the exec channel). The agent sends this as a Text message before/at start of session.
**DoD:** Agent sends resize message, container PTY reports the new dimensions (`stty size` shows 40 120).
**Test:** Unit test the resize message parsing.

### D4: Graceful shutdown (subagent)
**Files:** `gateway/src/main.rs` (only)
**Current:** SIGTERM kills the gateway, dropping all WebSocket connections immediately.
**Fix:** Install a `tokio::signal::ctrl_c()` + `SIGTERM` handler in `serve()`. On signal:
1. Stop accepting new connections.
2. Send `{"type":"shutdown", "reason":"gateway_restarting"}` to all active WebSocket PTY sessions.
3. Wait up to 10 seconds for connections to close.
4. Then exit.
Pods keep running (k3s doesn't kill them). Agents can RESUME after gateway restarts.
**DoD:** Send SIGTERM, verify all WebSocket clients receive shutdown message, gateway exits cleanly within 15s.
**Test:** Integration test with a mock WebSocket client.

### D5: File upload endpoint (subagent)
**Files:** `gateway/src/routes/files.rs` (new), `gateway/src/routes/mod.rs` (add route)
**Current:** No way to upload files to the workspace.
**Fix:** Add `POST /agent/:machine_id/files/upload?token=<connect_token>&path=<remote_path>`. Accept multipart/form-data. Write the file to the pod via `kubectl cp` or `kube exec -- cat > <path>`. Verify the token (same as PTY).
**DoD:** `curl -F "file=@local.txt" "https://gateway/agent/mach_01/files/upload?token=xxx&path=/home/dev/work/local.txt"` writes the file to the pod.
**Test:** Unit test the route handler with a mock body.

### D6: File download endpoint (subagent)
**Files:** `gateway/src/routes/files.rs` (same file as D5), `gateway/src/routes/mod.rs`
**Current:** No way to download files from the workspace.
**Fix:** Add `GET /agent/:machine_id/files/download?token=<connect_token>&path=<remote_path>`. Read the file from the pod via `kubectl cp` or `kube exec -- cat <path>`. Stream as HTTP response with `Content-Disposition: attachment`.
**DoD:** `curl -o local.txt "https://gateway/agent/mach_01/files/download?token=xxx&path=/home/dev/work/local.txt"` downloads the file.
**Test:** Unit test the route handler.

**Wave D DoD:**
- [ ] Persistent volumes survive pod restart
- [ ] Sessions expire automatically when TTL passes
- [ ] Terminal resize works
- [ ] SIGTERM triggers graceful shutdown with client notification
- [ ] File upload works via HTTP
- [ ] File download works via HTTP
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave E — Security Hardening (5 tasks)

**Goal:** Network isolation, session recording, key rotation, audit retention.

### E1: Network egress enforcement via NetworkPolicy (orchestrator-only — k8s networking)
**Files:** `gateway/src/machines/scheduler.rs`
**Current:** Pods can reach any external host. No NetworkPolicy created.
**Fix:** After creating a pod, create a `NetworkPolicy` that:
- Selects the pod by label (`machine-id=<pod_name>`)
- Denies all egress by default
- Allows egress to: DNS (kube-dns), the gateway IP, and tenant-specific allowlist hosts
The allowlist comes from a new `network_policies` table: `(tenant_id, host_pattern, port)`. Default allowlist: `github.com:443`, `crates.io:443`, `registry.npmjs.org:443`, `*.pypi.org:443`, `proxy.golang.org:443`.
**DoD:** Pod cannot `curl evil.com`. Pod can `curl github.com`. NetworkPolicy exists in k3s.
**Test:** Unit test the NetworkPolicy YAML generation.

### E2: Pod-to-pod isolation (subagent)
**Files:** `gateway/src/machines/scheduler.rs`
**Current:** All pods in `default` namespace can reach each other.
**Fix:** Add a default-deny NetworkPolicy for all pods labeled `app=stronghold-agent`. This prevents any pod from reaching any other pod. Only the gateway and DNS are reachable.
**DoD:** Pod A cannot `curl pod-B-ip`. Pod A can `curl gateway-ip`.
**Test:** Verify NetworkPolicy YAML is correct.

### E3: Session recording (subagent)
**Files:** `gateway/src/routes/pty.rs` (only)
**Current:** PTY proxy streams bytes but doesn't record them.
**Fix:** In `pty_proxy()`, append every byte (both directions: agent→container and container→agent) to a recording buffer. When the session ends (WebSocket closes or pod dies), write the recording to `/var/lib/stronghold/recordings/<machine_id>.cast` in asciinema v2 format:
```
{"version":2,"width":120,"height":40,"timestamp":1234567890}
[0.123456,"o","output text"]
[0.234567,"i","input text"]
```
Each line is a JSON array: `[timestamp, direction, content]` where direction is `"o"` (output) or `"i"` (input).
**DoD:** After a session, `/var/lib/stronghold/recordings/<machine_id>.cast` exists and can be replayed with `asciinema play`.
**Test:** Unit test the recording format.

### E4: Audit log retention (subagent)
**Files:** `gateway/src/audit/retention.rs` (new), `gateway/src/main.rs` (add background task)
**Current:** `audit_entries` table grows forever.
**Fix:** Add a background task that runs every 24 hours:
1. Deletes entries older than `retention_days` (default: 90, configurable).
2. Before deleting, exports them to `/var/lib/stronghold/audit/archive/<tenant_id>_<date>.jsonl` (one file per tenant per day).
3. The archive files are signed with the audit keys.
4. After archival, deletes the entries from the DB.
**DoD:** Entries older than `retention_days` are deleted. Archive file exists. Archive is signed.
**Test:** Unit test the retention logic with a mock DB.

### E5: TLS certificate rotation CLI (subagent)
**Files:** `cli/src/main.rs` (only — add `keys rotate-tls` subcommand)
**Current:** No way to rotate the TLS certificate.
**Fix:** Add `stronghold keys rotate-tls [--cn <domain>]` subcommand that:
1. Generates a new self-signed cert via `crypto::tls::generate_self_signed_cert_files()`.
2. Overwrites the existing `tls.crt` and `tls.key`.
3. Sends SIGHUP to the gateway process (or documents that a restart is needed).
4. Writes an audit entry.
**DoD:** Running `stronghold keys rotate-tls` replaces the cert files. Gateway needs restart to pick up the new cert.
**Test:** Unit test the CLI command parsing.

**Wave E DoD:**
- [ ] Pods cannot reach unauthorized external hosts
- [ ] Pods cannot reach each other
- [ ] Session recordings are saved in asciinema format
- [ ] Audit entries are archived and pruned after retention period
- [ ] TLS cert rotation CLI works
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave F — Phone & Approval (5 tasks)

**Goal:** Real push for anomalies/quorum, better mobile verification without a custom app.

### F1: Wire push_anomaly and push_quorum into PTY proxy (orchestrator-only — security path)
**Files:** `gateway/src/routes/pty.rs`
**Current:** `push_anomaly()` is defined but never called. Quorum requests sit in DB without ntfy push.
**Fix:**
1. In the anomaly detection branch of `pty_proxy()`: after writing the audit entry, call `crate::push::ntfy::push_anomaly(&tenant_id, &machine_id, &p.message, &state.db)`.
2. In the quorum branch: after inserting the `pending_sessions` row, call a new `crate::push::ntfy::push_quorum_request(&tenant_id, &quorum_id, &cmd, &state.db)` function. Add this function to `push/ntfy.rs` — it pushes an ntfy notification with action buttons: Approve → opens `https://gateway/phone/decide?request_id=<quorum_id>&decision=approve`, Deny → opens `...&decision=deny`.
3. Both push functions use `send_encrypted_or_fallback()` for E2E encryption.
**DoD:** Running `curl evil.com` in PTY triggers a phone push. Running `rm -rf /tmp/test` triggers a quorum push with Approve/Deny buttons.
**Test:** Verify push functions are called (mock or log check).

### F2: Signed push verification (subagent)
**Files:** `gateway/src/push/ntfy.rs` (only)
**Current:** ntfy notifications are E2E encrypted but there's no way for the phone to verify the push came from the legitimate gateway (a MITM could inject a fake ntfy message).
**Fix:** Add a `X-Stronghold-Sig` header to every push notification. The value is `ed25519:<base64(signature)>` where the signature is over `topic || title || message || timestamp`. The phone's enrollment page stores the gateway's Ed25519 public key. The PWA's JavaScript verifies the signature before showing the approval UI. If verification fails, show a red warning "Unverified notification — possible MITM".
**DoD:** Every ntfy push includes a valid `X-Stronghold-Sig` header. The PWA verifies it before showing the approval card.
**Test:** Unit test the signature generation and verification.

### F3: QR code enrollment (subagent)
**Files:** `phone/enroll.html` (only)
**Current:** Enrollment requires manually typing the setup password and verifying the SEV-SNP measurement.
**Fix:** Add a QR code to the `/setup` page that encodes:
```json
{
  "url": "https://gateway:8443",
  "tenant_id": "tenant_01HXYZ...",
  "setup_password": "AbCdEf...",
  "gateway_ed25519_pubkey": "base64:...",
  "sev_snp_measurement": "sha256:..."
}
```
The phone user scans this QR with their camera (Safari/Chrome on iOS/Android both support QR scanning from the browser URL bar). The QR opens the enrollment URL with all parameters pre-filled. The phone verifies the measurement matches `docs/MEASUREMENTS/v1.0.txt` and the pubkey matches before starting the WebAuthn ceremony.
**DoD:** Scanning the QR code on the `/setup` page opens the enrollment flow with all fields pre-filled. No manual typing needed.
**Test:** Verify QR code renders. Verify URL parameters are parsed.

### F4: iOS Web Push via PWA (subagent)
**Files:** `phone/enroll.html` (only), `phone/sw.js` (only)
**Current:** iOS doesn't receive push notifications when the browser is closed. Users must keep the tab open.
**Fix:** Use the Web Push API (available on iOS 16.4+ for installed PWAs):
1. In `sw.js`, add a `push` event listener that calls `self.registration.showNotification()`.
2. In `enroll.html`, add a "Enable Push Notifications" button that calls `Notification.requestPermission()` then subscribes via `pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: <vapid_public_key> })`.
3. Send the subscription to the gateway via `POST /phone/push-subscribe`.
4. The gateway stores the subscription and sends pushes via the Web Push API (using the `web-push` crate or HTTP API) instead of (or in addition to) ntfy.
5. Add `POST /phone/push-subscribe` endpoint to `routes/phone.rs`.
**Note:** This requires a VAPID key pair. Generate at startup, store in `/var/lib/stronghold/keys/vapid.key` and `.pub`.
**DoD:** On iOS 16.4+ with an installed PWA, push notifications arrive even when Safari is closed. Tapping the notification opens the approval page.
**Test:** Verify subscription flow works. Verify push message format.

### F5: Session preview in approval request (subagent)
**Files:** `gateway/src/routes/agent.rs` (add context to ORDER), `gateway/src/sessions/manager.rs` (store context), `phone/enroll.html` (display context)
**Current:** Approval request shows `image`, `ttl`, `reason`. No context about what the agent will do.
**Fix:**
1. Add an optional `context` field to `OrderRequest`: `{ "image": "...", "ttl_secs": 3600, "reason": "...", "context": { "repo": "github.com/me/proj", "branch": "feature-x", "plan": "fix auth bug, run tests" } }`.
2. Store the context in `pending_sessions` as a JSON string in the `reason` field (or add a `context` column).
3. Include the context in the SSE `approval_request` event.
4. The phone PWA displays the context as a card before the Approve/Deny buttons.
**DoD:** Agent sends ORDER with context. Phone displays the context in the approval card.
**Test:** Verify context flows from ORDER to SSE event.

**Wave F DoD:**
- [ ] Anomaly detection triggers phone push
- [ ] Quorum requests trigger phone push with Approve/Deny buttons
- [ ] Push notifications are signed and verified by the phone
- [ ] QR code enrollment works (scan → auto-fill → verify → WebAuthn)
- [ ] iOS Web Push works for installed PWAs (or documented as limitation if VAPID is too complex)
- [ ] Session context is displayed in the approval card
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave G — Operations & CLI (6 tasks)

**Goal:** Config file, CLI-to-gateway integration, Vultr API, graceful operations.

### G1: Configuration file (orchestrator-only — affects all modules)
**Files:** `gateway/src/config.rs` (new), `gateway/src/main.rs`, `gateway/Cargo.toml`
**Current:** All paths hardcoded: `/var/lib/stronghold/stronghold.db`, `/var/lib/stronghold/keys/`, `0.0.0.0:8443`, `localhost` RP ID.
**Fix:** Create `gateway/src/config.rs` with a `Config` struct:
```rust
pub struct Config {
    pub bind_addr: String,           // default: "0.0.0.0:8443"
    pub data_dir: String,            // default: "/var/lib/stronghold"
    pub db_path: String,             // default: "{data_dir}/stronghold.db"
    pub keys_dir: String,            // default: "{data_dir}/keys"
    pub recordings_dir: String,      // default: "{data_dir}/recordings"
    pub rp_id: String,               // default: "localhost"
    pub rp_origin: String,           // default: "https://localhost:8443"
    pub ntfy_url: String,            // default: "http://localhost:8090"
    pub retention_days: u32,         // default: 90
    pub dev: bool,                   // default: false
}
```
Load from `stronghold.toml` (if exists), then env vars (`STRONGHOLD_*`), then defaults. Pass `Config` through to `serve()` and all modules that use hardcoded paths.
**DoD:** `stronghold.toml` with `[server] bind_addr = "0.0.0.0:9443"` changes the bind address. No hardcoded paths remain in `main.rs`.
**Test:** Unit test config loading from TOML + env vars.

### G2: Admin API routes — tenant, credentials, tokens (subagent)
**Files:** `gateway/src/routes/admin.rs` (only)
**Current:** Only `POST /admin/tenant` and `GET /admin/tenant/:id` exist. CLI expects 8+ routes.
**Fix:** Add these routes to `admin.rs`:
- `GET /admin/tenant` — list all tenants
- `DELETE /admin/tenant/:id` — archive tenant
- `GET /admin/credentials?tenant=<id>` — list credentials
- `DELETE /admin/credentials/:id` — revoke credential
- `GET /admin/agent-token?tenant=<id>` — list agent tokens
- `DELETE /admin/agent-token/:id` — revoke token
- `POST /admin/agent-token` — mint token (body: `{tenant_id, scope, ttl_secs}`)
- `POST /admin/quotas` — set quotas (body: `{tenant_id, max_concurrent_machines, ...}`)
All routes require admin authentication (bearer token from `STRONGHOLD_ADMIN_TOKEN` env var).
**DoD:** All 8 routes return correct data. `curl` works for each.
**Test:** Integration test each route.

### G3: Admin API routes — audit, keys, workers, images (subagent)
**Files:** `gateway/src/routes/admin.rs` (same file as G2, or split)
**Current:** No audit, keys, worker, or image management routes.
**Fix:** Add:
- `GET /admin/audit/verify?tenant=<id>` — run verify_tenant, return report
- `GET /admin/audit/export?tenant=<id>&from=<ts>&to=<ts>` — export audit log
- `POST /admin/keys/rotate-audit` — rotate audit keys
- `POST /admin/keys/rotate-push` — rotate push keys
- `GET /admin/worker` — list workers (calls `machines::worker::list()`)
- `POST /admin/worker` — add worker (calls `machines::worker::add()`)
- `POST /admin/image/build` — build image (calls `images::builder::build()`)
- `GET /admin/image` — list images
**DoD:** All routes work. CLI can call them.
**Test:** Integration test each route.

### G4: CLI HTTP client (subagent)
**Files:** `cli/src/main.rs` (only)
**Current:** CLI has subcommands but the HTTP calls are stubs or assume routes that don't exist.
**Fix:** Wire each CLI subcommand to call the corresponding `/admin/*` route (from G2+G3) via reqwest. Handle errors gracefully. Use the `--url` flag or `STRONGHOLD_URL` env var for the gateway URL. Use `--admin-token` or `STRONGHOLD_ADMIN_TOKEN` for auth.
**DoD:** `stronghold tenant list` returns real tenants. `stronghold agent-token mint --tenant <id> --ttl 3600` returns a real token. All CLI commands work end-to-end.
**Test:** Integration test (requires running gateway — skip if not available).

### G5: Vultr API integration for VPS escalation (subagent)
**Files:** `gateway/src/machines/escalation.rs` (only), `gateway/Cargo.toml` (add `reqwest` if not already)
**Current:** `boot_vps()` returns a stub. `destroy_vps()` does nothing.
**Fix:** Implement real Vultr API calls:
1. `boot_vps()`: `POST https://api.vultr.com/v2/instances` with body: `{ "region": "ewr", "plan": "vc2-2c-4gb", "os_id": 187, "label": "stronghold-<machine_id>", "user_data": "<cloud-init script>" }`. The cloud-init script installs k3s worker, joins the cluster, pulls the OCI image. Headers: `Authorization: Bearer <VULTR_API_KEY>`.
2. `destroy_vps()`: `DELETE https://api.vultr.com/v2/instances/<instance_id>`.
3. `snapshot_volumes()`: `POST https://api.vultr.com/v2/instances/<instance_id>/snapshots` or use Vultr Block Storage snapshots.
4. API key from `VULTR_API_KEY` env var.
**DoD:** Calling `boot_vps()` creates a real Vultr instance. Calling `destroy_vps()` destroys it.
**Test:** Unit test the API request body construction (don't actually call the API in tests).

### G6: Graceful shutdown — pod drain (subagent)
**Files:** `gateway/src/main.rs` (only — extends D4)
**Current:** D4 handles WebSocket notification but doesn't drain k3s pods.
**Fix:** In the shutdown handler, after notifying WebSocket clients:
1. List all active pods via `scheduler::list_pods()`.
2. For each pod, send a `SIGTERM` via `kube exec` (or delete the pod with `GracePeriodSeconds: 30`).
3. Wait up to 30 seconds for pods to terminate.
4. If pods don't terminate, force-delete.
**DoD:** SIGTERM → all pods terminate within 30s → gateway exits.
**Test:** Integration test (requires k3s).

**Wave G DoD:**
- [ ] `stronghold.toml` config file works
- [ ] All `/admin/*` routes exist and return correct data
- [ ] CLI commands work end-to-end against the gateway
- [ ] Vultr API integration creates/destroys real VPS instances
- [ ] Graceful shutdown drains k3s pods
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave H — Integration & Ecosystem (3 tasks)

**Goal:** MCP server, webhook system, agent SDK.

### H1: MCP server (subagent)
**Files:** `gateway/src/mcp.rs` (new), `gateway/src/routes/mod.rs` (add route), `gateway/Cargo.toml`
**Current:** No MCP support. Agents must raw-curl the HTTP API.
**Fix:** Implement a minimal MCP server as an HTTP endpoint at `POST /mcp`:
1. Parse the MCP JSON-RPC request: `{"jsonrpc":"2.0","method":"tools/list","id":1}`.
2. `tools/list` returns the available tools:
   - `order_machine(image, ttl_secs, reason)` → returns machine_id + connect_token
   - `resume_machine(machine_id)` → returns connect_token
   - `release_machine(machine_id)` → returns success
   - `extend_machine(machine_id, additional_secs)` → returns success
   - `read_audit(machine_id, limit)` → returns recent audit entries
   - `upload_file(machine_id, path, content)` → returns success
   - `download_file(machine_id, path)` → returns file content
3. `tools/call` executes the tool by calling the corresponding internal function.
4. Authentication via the same agent bearer token.
**DoD:** An MCP-compatible agent (e.g., Claude with MCP) can call `order_machine` and get a machine_id back.
**Test:** Unit test the MCP request/response cycle.

### H2: Webhook system (subagent)
**Files:** `gateway/src/webhooks.rs` (new), `gateway/src/routes/mod.rs` (add `/admin/webhook` routes), `gateway/src/db/schema.sql` (add `webhooks` table)
**Current:** No way to notify external systems on lifecycle events.
**Fix:**
1. Add `webhooks` table: `(id, tenant_id, url, secret, events, created_at)`.
2. Add `POST /admin/webhook` (register), `GET /admin/webhook` (list), `DELETE /admin/webhook/:id` (delete).
3. Add `fire_webhook(tenant_id, event, payload)` function that queries the webhooks table, POSTs the event as JSON to each registered URL, signed with HMAC-SHA256 using the webhook secret.
4. Fire webhooks on: `session_started`, `session_ended`, `session_revoked`, `anomaly_detected`, `quorum_requested`, `quorum_approved`, `quorum_denied`.
5. Call `fire_webhook()` from the appropriate places in `sessions/manager.rs` and `routes/pty.rs`.
**DoD:** Register a webhook, trigger a session start, webhook URL receives a POST with the event payload.
**Test:** Unit test webhook registration and firing.

### H3: Agent convenience wrapper (subagent)
**Files:** `agent/stronghold.sh` (new), `agent/README.md` (new)
**Current:** Agents must construct raw curl commands.
**Fix:** Create a bash wrapper that agents source or call:
```bash
#!/usr/bin/env bash
# Usage: source stronghold.sh
# Then: stronghold_order "rust-nightly" 3600 "fix bug"
#       stronghold_shell  # opens PTY WebSocket
#       stronghold_release
#       stronghold_upload local.txt /home/dev/work/
#       stronghold_download /home/dev/work/output.txt

STRONGHOLD_URL="${STRONGHOLD_URL:-https://localhost:8443}"
STRONGHOLD_TOKEN="${STRONGHOLD_TOKEN:?Set STRONGHOLD_TOKEN to your agent token}"

stronghold_order() {
  local image="$1" ttl="$2" reason="$3"
  curl -sk -X POST "$STRONGHOLD_URL/agent/order" \
    -H "Authorization: Bearer $STRONGHOLD_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"image\":\"$image\",\"ttl_secs\":$ttl,\"reason\":\"$reason\"}"
}

stronghold_shell() {
  local machine_id="$1" token="$2"
  # Use websocat or wscat for WebSocket PTY
  websocat "wss://$STRONGHOLD_URL/agent/$machine_id/pty?token=$token"
}
# ... etc
```
**DoD:** `source stronghold.sh && stronghold_order "alpine" 300 "test"` returns a JSON response.
**Test:** Shellcheck the script. Verify curl commands match the API.

**Wave H DoD:**
- [ ] MCP server responds to `tools/list` and `tools/call`
- [ ] Webhook registration and firing works
- [ ] Agent convenience wrapper works
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave I — Observability (3 tasks)

**Goal:** Structured logging, Grafana dashboards, alert rules.

### I1: Structured logging with file output (subagent)
**Files:** `gateway/src/main.rs` (only)
**Current:** `tracing` logs to stdout. No file output, no level configuration.
**Fix:**
1. Add `tracing-appender` to Cargo.toml.
2. Configure `tracing_subscriber` with both a stdout layer (for systemd/journal) and a file layer (rolling daily to `/var/lib/stronghold/logs/stronghold.log`).
3. Add `RUST_LOG` env var support for level configuration (default: `stronghold_gateway=info,tower_http=info`).
4. Log file rotates daily, keeps 7 days.
**DoD:** Logs appear in both stdout and `/var/lib/stronghold/logs/stronghold.log`. `RUST_LOG=debug` increases verbosity.
**Test:** Verify log file is created and written to.

### I2: Grafana dashboard JSON (subagent)
**Files:** `setup/monitoring/stronghold-dashboard.json` (new), `docs/OBSERVABILITY.md` (new)
**Current:** Prometheus scrapes `/metrics` but no dashboards exist.
**Fix:** Create a Grafana dashboard JSON with panels for:
- Active sessions over time (gauge)
- Pending approvals over time (gauge)
- Audit entries total (counter, rate panel)
- Request latency histogram (from TraceLayer — needs a histogram metric)
- 503 rejections rate
- Pod count by status
Include the JSON file and a doc explaining how to import it into Grafana.
**DoD:** Dashboard JSON imports cleanly into Grafana and shows real data.
**Test:** Validate JSON structure.

### I3: Alert rules (subagent)
**Files:** `setup/monitoring/alerts.yml` (new), `docs/OBSERVABILITY.md` (update)
**Current:** No alert rules.
**Fix:** Create Prometheus alert rules:
- `StrongholdNoActiveSessions` — no active sessions for >1h (info)
- `StrongholdHighPendingApprovals` — >5 pending approvals for >5m (warning)
- `StrongholdHighAnomalyRate` — >10 anomaly_detected events in 5m (critical)
- `StrongholdGatewayDown` — no `/metrics` scrape for 2m (critical)
- `StrongholdHigh503Rate` — >10 503 responses in 5m (warning)
Include YAML file and update docs.
**DoD:** Alert rules are valid Prometheus YAML. Documented in OBSERVABILITY.md.
**Test:** Validate YAML syntax.

**Wave I DoD:**
- [ ] Logs go to both stdout and rolling file
- [ ] Grafana dashboard JSON imports and displays real metrics
- [ ] Alert rules are valid and documented
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Final: v1.0-rc Tag

After all waves pass:
1. Update `CHANGELOG.md` — add `[1.0.0-rc]` section
2. Update `README.md` — badge to `rc`
3. Update `SECURITY.md` — update status
4. Create `docs/releases/v1.0.0-rc.md`
5. Tag `v1.0.0-rc`
6. Append final worklog entry

---

## Subagent Prompt Template

```
Task ID: <ID>

You are implementing ONE feature in the Stronghold project.

FILE SCOPE: You may ONLY modify these files:
- <file 1>
- <file 2 if needed>
Do NOT touch any other files.

CURRENT STATE: <what the code does now, with line numbers>

FIX: <precise description>

CONSTRAINTS:
- Do NOT touch files outside the scope listed above.
- Do NOT change function signatures unless explicitly told to.
- Run: cd /root/stronghold && cargo build --workspace --features no-sev-snp
- Run: cd /root/stronghold && cargo clippy --workspace --features no-sev-snp -- -D warnings
- Run: cd /root/stronghold && cargo test --workspace --features no-sev-snp
- All three must pass before you return.
- Commit ONLY your files: git add <file1> <file2> && git commit -m "<ID>: <summary>"
- Push: git push origin main

DOD: <what "done" looks like>
TESTS: <what tests to write>

Return: files changed, test count, any issues.
```

---

## Execution Order

```
Wave D (6 tasks, 2 orchestrator + 4 subagent):
  D1 (orchestrator) → D2,D3,D4 parallel → D5,D6 parallel → gate → push

Wave E (5 tasks, 1 orchestrator + 4 subagent):
  E1 (orchestrator) → E2,E3,E4,E5 parallel → gate → push

Wave F (5 tasks, 1 orchestrator + 4 subagent):
  F1 (orchestrator) → F2,F3,F4,F5 parallel → gate → push

Wave G (6 tasks, 1 orchestrator + 5 subagent):
  G1 (orchestrator) → G2,G3 parallel → G4,G5,G6 parallel → gate → push

Wave H (3 tasks, all subagent):
  H1,H2,H3 parallel → gate → push

Wave I (3 tasks, all subagent):
  I1,I2,I3 parallel → gate → push

Final: tag v1.0.0-rc
```

Total: 28 tasks across 6 waves. Estimated 42 subagent invocations + 6 orchestrator tasks.
