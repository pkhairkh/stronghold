# Stronghold — Orchestrator Transformation Prompt

> 5 waves, 26 surgical tasks. Transforms Stronghold from a secure shell provisioner into an agent orchestration platform with task model, credential vault, git flow, workflow engine, and multi-agent coordination.
>
> **The orchestrator agent MUST NOT return until ALL wave DoDs pass.** If a task fails, fix it. If a subagent produces broken code, fix it or re-brief. If the build breaks, fix it. Loop until green. No excuses.

---

## 0. Orchestrator Protocol

### Execution loop
```
for each wave J..O:
    1. READ worklog.md + this prompt for the wave
    2. PLAN: decide orchestrator-only vs delegated tasks
    3. EXECUTE: spawn subagents (max 4 parallel), do own tasks serially
    4. REVIEW: read every changed file, run build+clippy+test
    5. COMMIT: one commit per task (format: "J1: <summary>")
    6. PUSH: push after all tasks in the wave pass
    7. GATE: run wave DoD — if ANY check fails, loop back to step 3
    8. NEXT WAVE
```

### Hard rules
- One task per subagent. Each subagent gets: task ID, file scope (1-3 files), current state, fix, DoD, test requirements.
- Orchestrator does NOT delegate: DB schema changes, crypto/credential encryption, workflow DAG executor, multi-agent coordination primitives.
- After each task: `cargo build && cargo clippy -- -D warnings && cargo test` on dev box.
- After each wave: push + sync dev box + run wave DoD.
- **The orchestrator agent MUST NOT return until ALL wave DoDs pass.** If a wave DoD fails, the orchestrator fixes the issue (either directly or by re-briefing a subagent) and re-runs the gate. This loop continues until green. No partial completion. No "I'll come back to it."

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
All three must exit 0. If any fails, the task is NOT done — fix it before moving to the next task.

### Wave gate (after each wave)
```bash
# Build + clippy + test
cargo build --workspace --features no-sev-snp
cargo clippy --workspace --features no-sev-snp -- -D warnings
cargo test --workspace --features no-sev-snp

# Wave-specific DoD checks (see each wave)
# All must pass before proceeding to the next wave
```

---

## Wave J — Task Model & Structured I/O (5 tasks)

**Goal:** Replace "session = raw PTY" with "task = structured work unit with lifecycle, exec, and reprompt."

**Entry condition:** v0.10.1-beta tag, 283 tests pass.

### J1: DB schema — tasks, workflows, task_outputs (orchestrator-only)

**Files:** `gateway/src/db/schema.sql` (add tables), `gateway/src/db/mod.rs` (add migration 003)
**Current:** No task/workflow tables exist. Sessions are the only unit of work.
**Fix:** Add these tables to schema.sql:

```sql
CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    machine_id      TEXT,
    parent_task_id  TEXT,
    workflow_run_id TEXT,
    status          TEXT DEFAULT 'queued',
    spec            TEXT NOT NULL,
    result          TEXT,
    created_at      TEXT NOT NULL,
    started_at      TEXT,
    finished_at     TEXT,
    error           TEXT,
    retry_count     INTEGER DEFAULT 0,
    max_retries     INTEGER DEFAULT 3,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id),
    FOREIGN KEY (machine_id) REFERENCES machines(id)
);

CREATE TABLE IF NOT EXISTS workflows (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    name            TEXT NOT NULL,
    dag             TEXT NOT NULL,
    status          TEXT DEFAULT 'draft',
    created_at      TEXT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

CREATE TABLE IF NOT EXISTS workflow_runs (
    id              TEXT PRIMARY KEY,
    workflow_id     TEXT NOT NULL,
    tenant_id       TEXT NOT NULL,
    status          TEXT DEFAULT 'pending',
    current_steps   TEXT,
    completed_steps TEXT,
    started_at      TEXT,
    finished_at     TEXT,
    result          TEXT,
    FOREIGN KEY (workflow_id) REFERENCES workflows(id),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

CREATE TABLE IF NOT EXISTS task_outputs (
    task_id         TEXT NOT NULL,
    key             TEXT NOT NULL,
    value           TEXT,
    artifact_path   TEXT,
    PRIMARY KEY (task_id, key),
    FOREIGN KEY (task_id) REFERENCES tasks(id)
);
```

Add migration 003 to `db/mod.rs` (same pattern as migration 002 — check `PRAGMA table_info` before creating).

**DoD:** All 4 tables exist in a fresh DB. `init_memory_pool()` creates them. Existing tests still pass.
**Test:** Add test `test_tasks_tables_exist` that verifies all 4 tables are created.
**Context budget:** ~60 lines in schema.sql, ~30 lines in mod.rs.

### J2: Structured command execution — POST /agent/:machine_id/exec (subagent)

**Files:** `gateway/src/routes/exec.rs` (new), `gateway/src/routes/mod.rs` (add route + mod)
**Current:** Agents can only interact via raw PTY WebSocket. No way to run a command and get structured output.
**Fix:**
1. Create `gateway/src/routes/exec.rs` with handler:
```rust
pub async fn exec_command(
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,  // reuse token verification
    State(state): State<AppState>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, (StatusCode, String)>
```
2. `ExecRequest`: `{ cmd: String, args: Vec<String>, cwd: Option<String>, timeout_secs: u64, env: HashMap<String,String> }`
3. The handler:
   - Verifies the token (same as PTY — query DB for connect_token_hash)
   - Runs the command in the pod via `kube exec` (use `scheduler::open_pty` pattern but non-interactive — exec with specific command, capture stdout/stderr/exit_code)
   - Returns `ExecResponse`: `{ exit_code: i32, stdout: String, stderr: String, duration_ms: u64, audit_seq: i64 }`
   - Writes an audit entry: event `cmd_exec`, payload `{cmd, exit_code, duration_ms}`
4. Register route in `mod.rs`: `.route("/agent/:machine_id/exec", axum::routing::post(exec::exec_command))`

For the kube exec, use `kube::Api::exec()` with a specific command (not interactive shell):
```rust
let ap = AttachParams::default()
    .stdin(false)
    .stdout(true)
    .stderr(true)
    .tty(false)
    .command(vec![req.cmd].into_iter().chain(req.args).collect());
let mut exec = pods.exec(&machine_id, req.cmd, &ap).await?;
// Read stdout/stderr to completion, collect exit code
```

**DoD:** `POST /agent/mach_01/exec` with `{"cmd":"echo","args":["hello"],"timeout_secs":10}` returns `{"exit_code":0,"stdout":"hello\n","stderr":"","duration_ms":42,"audit_seq":N}`.
**Test:** Unit test the ExecRequest/ExecResponse serialization. Integration test with mock pod (skip if no k3s).
**Context budget:** ~120 lines in new file, 2 lines in mod.rs.

### J3: Task lifecycle — POST /agent/task, GET /agent/task/:id (subagent)

**Files:** `gateway/src/routes/tasks.rs` (new), `gateway/src/routes/mod.rs` (add routes)
**Current:** No task concept. Only ORDER (which creates a session/machine).
**Fix:**
1. Create `gateway/src/routes/tasks.rs`:
2. `POST /agent/task` — create a task:
   - Request: `{ instruction: String, image: String, ttl_secs: u64, context: Option<serde_json::Value>, parent_task_id: Option<String>, workflow_run_id: Option<String> }`
   - Creates a task row with status `queued`
   - If `machine_id` is not provided, creates a session (calls ORDER internally) and links the task to the machine
   - If `parent_task_id` is provided, links as sub-task
   - Returns: `{ task_id, machine_id, status: "queued" }`
3. `GET /agent/task/:id` — get task status:
   - Returns: `{ id, status, spec, result, created_at, started_at, finished_at, error, retry_count }`
4. `POST /agent/task/:id/result` — agent submits task result:
   - Request: `{ exit_code: i32, stdout: String, stderr: String, summary: String, artifacts: Vec<ArtifactRef> }`
   - Updates task status to `completed` (exit_code 0) or `failed` (non-zero)
   - Writes audit entry
5. Register routes in `mod.rs`:
   - `.route("/agent/task", axum::routing::post(tasks::create_task))`
   - `.route("/agent/task/:id", axum::routing::get(tasks::get_task))`
   - `.route("/agent/task/:id/result", axum::routing::post(tasks::submit_result))`

**DoD:** `POST /agent/task` with `{"instruction":"echo hello","image":"alpine","ttl_secs":300}` creates a task and returns a task_id. `GET /agent/task/:id` returns the task status.
**Test:** Unit test task creation and status retrieval with in-memory DB.
**Context budget:** ~150 lines in new file, 3 lines in mod.rs.

### J4: Mid-session reprompt — POST /agent/:machine_id/instruct (orchestrator-only — core feature)

**Files:** `gateway/src/routes/instruct.rs` (new), `gateway/src/routes/mod.rs` (add route)
**Current:** Once a session starts, the agent is on its own. No way to inject new instructions.
**Fix:**
1. Create `gateway/src/routes/instruct.rs`:
2. `POST /agent/:machine_id/instruct` — inject a new instruction into a running session:
   - Request: `{ instruction: String, context: Option<serde_json::Value>, mode: String, priority: Option<String> }`
   - `mode` is one of: `"pty"` (inject as PTY text), `"control"` (send on control channel), `"task"` (queue as sub-task)
3. For `mode: "pty"`:
   - Get the PTY handle for this machine_id (need a registry — add a `HashMap<String, mpsc::Sender<Vec<u8>>>` in AppState, or use a tokio::sync::broadcast channel)
   - Write the instruction text to the PTY stdin as: `\n# Stronghold Instruction: {instruction}\n`
   - The agent's runtime sees this as a new comment in the terminal
4. For `mode: "control"`:
   - Send a JSON message on a control WebSocket channel (`/agent/:machine_id/control`)
   - The message is: `{"type":"instruct","instruction":"...","context":{...},"priority":"high"}`
   - Open a new WebSocket route for the control channel
5. For `mode: "task"`:
   - Create a new Task with `parent_task_id` set to the current task for this machine
   - Status `queued` — the agent picks it up when the current task finishes
6. Write audit entry: event `instruct_received`, payload `{mode, instruction_snippet}`
7. Register routes:
   - `.route("/agent/:machine_id/instruct", axum::routing::post(instruct::inject))`
   - `.route("/agent/:machine_id/control", axum::routing::get(instruct::control_ws))`

For the PTY handle registry, add to `AppState`:
```rust
pub struct AppState {
    pub db: Pool<SqliteConnectionManager>,
    pub audit_keys: AuditKeys,
    pub push_keys: PushKeys,
    pub pty_registry: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>>,  // machine_id → stdin sender
}
```
The `pty_proxy` function registers its stdin sender in the registry on connect and removes it on disconnect.

**DoD:** `POST /agent/mach_01/instruct` with `{"instruction":"Stop. Try a different approach.","mode":"pty"}` injects the text into the running PTY. The agent sees it.
**Test:** Unit test the instruction parsing and audit entry creation. Integration test with mock PTY.
**Context budget:** ~200 lines in new file, 2 lines in mod.rs, ~10 lines in mod.rs for AppState change.

### J5: Task status SSE stream (subagent)

**Files:** `gateway/src/routes/tasks.rs` (extend J3 file)
**Current:** No way to monitor task progress in real-time.
**Fix:** Add `GET /agent/task/:id/stream` — SSE stream that:
1. Returns the current task status immediately
2. Polls the DB every 500ms for status changes
3. Emits SSE events: `task_created`, `task_started`, `task_completed`, `task_failed`, `task_cancelled`
4. Emits heartbeat every 30s
5. Closes stream when task reaches a terminal state (completed/failed/cancelled)

Register route: `.route("/agent/task/:id/stream", axum::routing::get(tasks::stream_task))`

**DoD:** Creating a task and subscribing to the SSE stream emits `task_created` immediately. When the task completes, `task_completed` is emitted.
**Test:** Unit test the SSE stream with a mock DB.
**Context budget:** ~80 lines added to tasks.rs, 1 line in mod.rs.

**Wave J DoD:**
- [ ] `tasks`, `workflows`, `workflow_runs`, `task_outputs` tables exist in DB
- [ ] `POST /agent/:machine_id/exec` runs a command and returns structured output
- [ ] `POST /agent/task` creates a task with lifecycle (queued → running → completed/failed)
- [ ] `GET /agent/task/:id` returns task status
- [ ] `POST /agent/task/:id/result` submits task result
- [ ] `POST /agent/:machine_id/instruct` injects mid-session instructions (3 modes)
- [ ] `GET /agent/task/:id/stream` emits SSE events on status changes
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave K — Credential Vault (4 tasks)

**Goal:** Secure credential storage and injection for agents.

### K1: Credential vault DB schema + encryption (orchestrator-only — crypto)

**Files:** `gateway/src/db/schema.sql` (add table), `gateway/src/db/mod.rs` (migration 004), `gateway/src/crypto/vault.rs` (new)
**Current:** No credential storage. Agents have nowhere to securely fetch SSH keys, API tokens, etc.
**Fix:**
1. Add to schema.sql:
```sql
CREATE TABLE IF NOT EXISTS credentials (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    name            TEXT NOT NULL,
    kind            TEXT NOT NULL,
    encrypted_value BLOB NOT NULL,
    nonce           BLOB NOT NULL,
    env_var         TEXT,
    mount_path      TEXT,
    created_at      TEXT NOT NULL,
    rotated_at      TEXT,
    UNIQUE(tenant_id, name),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);
```
2. Create `gateway/src/crypto/vault.rs`:
   - `derive_tenant_key(tenant_id, audit_keys) -> [u8; 32]` — HKDF from the audit Ed25519 secret key + tenant_id
   - `encrypt(plaintext, tenant_key) -> (ciphertext, nonce)` — AES-256-GCM
   - `decrypt(ciphertext, nonce, tenant_key) -> plaintext` — AES-256-GCM
   - The tenant key is derived in memory, never stored
3. Add migration 004 to mod.rs

**DoD:** `credentials` table exists. `vault::encrypt` then `vault::decrypt` returns the original plaintext. Different tenant_ids produce different keys.
**Test:** Unit test encrypt/decrypt round-trip, wrong key rejection, wrong nonce rejection.
**Context budget:** ~20 lines in schema.sql, ~30 lines in mod.rs, ~100 lines in vault.rs.

### K2: Credential CRUD — POST/GET/DELETE /admin/credentials (subagent)

**Files:** `gateway/src/routes/admin.rs` (extend), `gateway/src/routes/mod.rs` (add routes)
**Current:** No credential management routes.
**Fix:**
1. `POST /admin/credentials` — store a credential:
   - Request: `{ tenant_id, name, kind, value, env_var?, mount_path? }`
   - Encrypts the value with the tenant key
   - Stores in `credentials` table
   - Returns: `{ id, name, kind, created_at }`
2. `GET /admin/credentials?tenant=<id>` — list credentials (returns metadata, NOT values):
   - Returns: `[{ id, name, kind, env_var, mount_path, created_at, rotated_at }]`
3. `GET /admin/credentials/:id` — get credential value (DECRYPTS):
   - Returns: `{ id, name, kind, value, env_var, mount_path }`
   - Writes audit entry: event `credential_accessed`
4. `DELETE /admin/credentials/:id` — revoke credential
5. `POST /admin/credentials/:id/rotate` — rotate credential value:
   - Request: `{ value }` — new value, re-encrypts, updates `rotated_at`
6. Register routes in `mod.rs`

**DoD:** Create a credential, list it (no value), get it (with value), rotate it, delete it. All operations audit-logged.
**Test:** Integration test the full CRUD cycle with in-memory DB.
**Context budget:** ~150 lines in admin.rs, 5 lines in mod.rs.

### K3: Agent credential access — GET /agent/:machine_id/credentials/:name (subagent)

**Files:** `gateway/src/routes/agent.rs` (extend), `gateway/src/routes/mod.rs` (add route)
**Current:** Agents have no way to access credentials.
**Fix:**
1. `GET /agent/:machine_id/credentials/:name`:
   - Verifies agent token (same as other agent routes)
   - Looks up `machine_id` → `tenant_id` from `machines` table
   - Queries `credentials` table for `(tenant_id, name)`
   - Decrypts the value
   - Writes audit entry: event `credential_accessed`, payload `{name, machine_id}` (NOT the value)
   - Returns: `{ name, kind, value, env_var, mount_path }`
2. Register route: `.route("/agent/:machine_id/credentials/:name", axum::routing::get(agent::get_credential))`

**DoD:** Agent can fetch a credential by name. Credential value is never logged. Audit entry records the access.
**Test:** Integration test with in-memory DB + encrypted credential.
**Context budget:** ~60 lines in agent.rs, 1 line in mod.rs.

### K4: Pod credential injection (subagent)

**Files:** `gateway/src/machines/scheduler.rs` (only)
**Current:** Pod spec has no credential injection. Agents must manually fetch credentials via API.
**Fix:**
1. After creating a pod, before returning from `schedule()`:
   - Query `credentials` table for all credentials belonging to this tenant
   - For credentials with `env_var` set: add as env vars in the pod spec
   - For credentials with `mount_path` set: create a Kubernetes Secret and mount it as a file
2. The env var approach is simpler — add them to the container spec:
```json
"env": [
  {"name": "GITHUB_TOKEN", "valueFrom": {"secretKeyRef": {"name": "stronghold-<tenant>", "key": "github-pat"}}}
]
```
3. Create the K8s Secret before creating the pod:
```rust
let secret = Secret {
    metadata: ObjectMeta { name: format!("stronghold-{}", tenant_id), ... },
    string_data: decrypted_credentials.iter().map(|c| (c.name.clone(), c.value.clone())).collect(),
    ..
};
secrets.create(&Default::default(), &secret).await?;
```
4. Decrypt the credential values (using `vault::decrypt` with the tenant key)

**DoD:** Pod created for a tenant with credentials has `GITHUB_TOKEN` env var set. `echo $GITHUB_TOKEN` in the pod returns the decrypted value.
**Test:** Unit test the env var injection logic. Integration test with mock k8s.
**Context budget:** ~80 lines in scheduler.rs.

**Wave K DoD:**
- [ ] `credentials` table exists with encrypted storage
- [ ] `crypto/vault.rs` encrypts/decrypts with per-tenant keys
- [ ] Admin can create/list/get/rotate/delete credentials
- [ ] Agent can fetch credentials by name via API
- [ ] Credentials are injected into pods as env vars at creation time
- [ ] Credential values are never logged in audit entries
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave L — Git Flow (5 tasks)

**Goal:** First-class git workflow API for agents.

### L1: Git clone — POST /agent/:machine_id/git/clone (subagent)

**Files:** `gateway/src/routes/git.rs` (new), `gateway/src/routes/mod.rs` (add route)
**Current:** Agents must manually run git commands in the PTY.
**Fix:**
1. Create `gateway/src/routes/git.rs`:
2. `POST /agent/:machine_id/git/clone`:
   - Request: `{ repo: String, branch: Option<String>, path: Option<String> }`
   - Verifies token (same as exec)
   - Fetches the `github-pat` credential (or `git-token`) from the credential vault for this tenant
   - Runs `git clone https://<token>@<repo> <path>` via `kube exec`
   - If `branch` is specified, runs `git checkout <branch>`
   - Returns: `{ exit_code, stdout, stderr, duration_ms, audit_seq }`
   - Writes audit entry: event `git_clone`, payload `{repo, branch}` (NOT the token)
3. Register route: `.route("/agent/:machine_id/git/clone", axum::routing::post(git::clone))`

**DoD:** `POST /agent/mach_01/git/clone` with `{"repo":"github.com/me/repo"}` clones the repo into the pod.
**Test:** Unit test the request/response serialization. Integration test with mock pod.
**Context budget:** ~100 lines in new file, 1 line in mod.rs.

### L2: Git branch/commit/push (subagent)

**Files:** `gateway/src/routes/git.rs` (extend L1 file), `gateway/src/routes/mod.rs` (add routes)
**Current:** No git operation endpoints.
**Fix:**
1. `POST /agent/:machine_id/git/branch`:
   - Request: `{ name: String, from: Option<String> }`
   - Runs `git checkout -b <name>` (or `git checkout -b <name> <from>`)
   - Returns structured output
2. `POST /agent/:machine_id/git/commit`:
   - Request: `{ message: String, files: Option<Vec<String>> }`
   - If `files` specified, runs `git add <files>` first; else `git add -A`
   - Runs `git commit -m <message>`
   - Returns: `{ exit_code, stdout, stderr, commit_sha: Option<String> }`
3. `POST /agent/:machine_id/git/push`:
   - Request: `{ remote: Option<String>, branch: Option<String> }`
   - Runs `git push <remote> <branch>` (defaults: origin, current branch)
   - Returns structured output
4. Register routes in `mod.rs`

**DoD:** Clone → branch → commit → push works as a sequence. Each step returns structured output.
**Test:** Unit test request/response serialization.
**Context budget:** ~120 lines in git.rs, 3 lines in mod.rs.

### L3: Git PR creation — POST /agent/:machine_id/git/pr (subagent)

**Files:** `gateway/src/routes/git.rs` (extend), `gateway/src/routes/mod.rs` (add route)
**Current:** No PR creation capability.
**Fix:**
1. `POST /agent/:machine_id/git/pr`:
   - Request: `{ title: String, body: Option<String>, base: String, head: String }`
   - Fetches `github-pat` credential from the vault
   - Calls GitHub API: `POST https://api.github.com/repos/<owner>/<repo>/pulls`
     - Extracts owner/repo from the git remote URL (run `git remote get-url origin` via exec)
     - Uses reqwest with `Authorization: token <pat>` header
   - Returns: `{ pr_number, pr_url, pr_state }`
   - Writes audit entry: event `git_pr_created`, payload `{title, base, head, pr_url}`
2. Register route: `.route("/agent/:machine_id/git/pr", axum::routing::post(git::create_pr))`

**DoD:** After clone → branch → commit → push, calling `git/pr` creates a real GitHub PR and returns the URL.
**Test:** Unit test the GitHub API request construction (mock the HTTP call).
**Context budget:** ~100 lines in git.rs, 1 line in mod.rs.

### L4: Git status — GET /agent/:machine_id/git/status (subagent)

**Files:** `gateway/src/routes/git.rs` (extend), `gateway/src/routes/mod.rs` (add route)
**Current:** No way to query repo status programmatically.
**Fix:**
1. `GET /agent/:machine_id/git/status`:
   - Runs `git status --porcelain=v2 --branch` via `kube exec`
   - Parses the output into structured JSON:
     ```json
     {
       "branch": "fix/auth-bug",
       "upstream": "origin/fix/auth-bug",
       "ahead": 2,
       "behind": 0,
       "staged": [{"path": "src/auth.rs", "status": "modified"}],
       "unstaged": [],
       "untracked": ["debug.log"]
     }
     ```
   - Returns the structured status
2. Also add `GET /agent/:machine_id/git/log`:
   - Runs `git log --oneline -10` via exec
   - Returns: `{ commits: [{sha, message}] }`
3. Register routes in `mod.rs`

**DoD:** `GET /agent/mach_01/git/status` returns structured repo status. `GET /agent/mach_01/git/log` returns recent commits.
**Test:** Unit test the output parsing.
**Context budget:** ~100 lines in git.rs, 2 lines in mod.rs.

### L5: Git audit logging (subagent)

**Files:** `gateway/src/routes/git.rs` (extend — add audit calls to all git endpoints)
**Current:** Git operations are not audit-logged.
**Fix:**
1. Every git endpoint writes an audit entry before returning:
   - `git_clone`: `{repo, branch, exit_code}`
   - `git_branch`: `{name, from, exit_code}`
   - `git_commit`: `{message, commit_sha, exit_code}`
   - `git_push`: `{remote, branch, exit_code}`
   - `git_pr_created`: `{title, base, head, pr_url}`
   - `git_status`: (no audit — read-only)
2. All audit entries include `machine_id` and `tenant_id` (looked up from machines table)
3. Credential tokens are NEVER included in audit payloads

**DoD:** Every git operation (except status/log which are read-only) creates an audit entry. `audit verify` shows git operations.
**Test:** Verify audit entries are written for each git operation.
**Context budget:** ~40 lines added across git.rs endpoints.

**Wave L DoD:**
- [ ] `POST /agent/:machine_id/git/clone` works with credential vault
- [ ] `POST /agent/:machine_id/git/branch` creates branches
- [ ] `POST /agent/:machine_id/git/commit` commits with message
- [ ] `POST /agent/:machine_id/git/push` pushes to remote
- [ ] `POST /agent/:machine_id/git/pr` creates a GitHub PR
- [ ] `GET /agent/:machine_id/git/status` returns structured status
- [ ] `GET /agent/:machine_id/git/log` returns recent commits
- [ ] All git operations are audit-logged (no credential values in logs)
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave M — Workflow Engine (4 tasks)

**Goal:** Multi-step agent workflows with DAG execution.

### M1: Workflow definition — POST /workflow, GET /workflow/:id (subagent)

**Files:** `gateway/src/routes/workflows.rs` (new), `gateway/src/routes/mod.rs` (add routes)
**Current:** No workflow concept.
**Fix:**
1. `POST /workflow` — define a workflow:
   - Request: `{ name: String, dag: DagSpec }`
   - `DagSpec` is JSON: `{ steps: [{ id, task: { instruction, image, ttl_secs }, depends_on: [step_id], condition: Option<String> }] }`
   - Stores in `workflows` table
   - Returns: `{ workflow_id, status: "draft" }`
2. `GET /workflow/:id` — get workflow definition
3. `GET /workflow` — list workflows for tenant
4. Register routes in `mod.rs`

**DoD:** Create a workflow with 3 steps (clone → fix → test). Retrieve it by ID. List workflows.
**Test:** Unit test workflow creation and retrieval.
**Context budget:** ~120 lines in new file, 3 lines in mod.rs.

### M2: Workflow run — POST /workflow/:id/run (orchestrator-only — execution logic)

**Files:** `gateway/src/routes/workflows.rs` (extend M1), `gateway/src/workflow/engine.rs` (new)
**Current:** No workflow execution.
**Fix:**
1. `POST /workflow/:id/run` — start a workflow run:
   - Creates a `workflow_runs` row with status `running`
   - Spawns a background task: `workflow::engine::execute(run_id, state)`
   - Returns: `{ run_id, status: "running" }`
2. Create `gateway/src/workflow/engine.rs`:
   - `execute(run_id, state)` — the DAG executor:
     a. Load the workflow DAG from DB
     b. Find all steps where `depends_on` is empty → these are ready to run
     c. For each ready step:
        - Create a Task (calls the task creation logic from J3)
        - Update `current_steps` in `workflow_runs`
        - Wait for task completion (poll DB or use channel)
        - On success: add step to `completed_steps`, evaluate conditions for next steps
        - On failure: if `retry_count < max_retries`, retry; else mark step as failed
     d. Repeat until all steps are completed or a step fails with no retries left
     e. Update `workflow_runs.status` to `completed` or `failed`
     f. Write audit entry: event `workflow_completed` or `workflow_failed`

**DoD:** Create a 2-step workflow (echo hello → echo world). Run it. Both steps execute. Run status becomes `completed`.
**Test:** Unit test the DAG evaluator with a mock DB.
**Context budget:** ~200 lines in engine.rs, ~30 lines in workflows.rs.

### M3: Workflow run status — GET /workflow/run/:id (subagent)

**Files:** `gateway/src/routes/workflows.rs` (extend)
**Current:** No way to check workflow run progress.
**Fix:**
1. `GET /workflow/run/:id` — get workflow run status:
   - Returns: `{ id, workflow_id, status, current_steps, completed_steps, started_at, finished_at, result }`
   - `current_steps` and `completed_steps` are JSON arrays of step IDs
2. `GET /workflow/run/:id/stream` — SSE stream:
   - Emits events when steps start/complete/fail
   - Emits `workflow_completed` or `workflow_failed` on terminal state
3. Register routes in `mod.rs`

**DoD:** After starting a workflow run, `GET /workflow/run/:id` shows which steps are running and which are completed.
**Test:** Unit test status retrieval.
**Context budget:** ~80 lines in workflows.rs, 2 lines in mod.rs.

### M4: Conditional branching and parallel execution (subagent)

**Files:** `gateway/src/workflow/engine.rs` (extend M2)
**Current:** DAG executor runs steps sequentially (M2 basic version).
**Fix:**
1. **Parallel execution**: when multiple steps have their dependencies met, launch them all concurrently (use `tokio::spawn` + `join_all`)
2. **Conditional branching**: evaluate `condition` field:
   - Condition is a JSON expression: `"clone.result.exit_code == 0"`
   - Parse the expression: `<step_id>.result.<field> <op> <value>`
   - Look up the step's result from `tasks.result` (JSON)
   - Only launch the step if the condition evaluates to true
3. **Error handling**: if a step fails and has no retries left:
   - Check if any downstream steps have `condition` that handles failure
   - If not, mark the entire workflow run as `failed`
   - If yes, skip the failed step and continue

**DoD:** Create a workflow with a conditional step (only runs if the previous step succeeded). Create a workflow with two parallel steps.
**Test:** Unit test conditional evaluation and parallel execution.
**Context budget:** ~100 lines in engine.rs.

**Wave M DoD:**
- [ ] Workflows can be defined with DAG structure
- [ ] `POST /workflow/:id/run` starts execution
- [ ] DAG executor runs steps in dependency order
- [ ] Parallel steps execute concurrently
- [ ] Conditional branching works
- [ ] Failed steps retry up to `max_retries`
- [ ] `GET /workflow/run/:id` shows real-time progress
- [ ] SSE stream emits step lifecycle events
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave N — Agent SDK & Multi-Agent (4 tasks)

**Goal:** Agent convenience layer and multi-agent coordination.

### N1: Agent bash SDK (subagent)

**Files:** `agent/stronghold-agent.sh` (new), `agent/README.md` (new)
**Current:** Agents must construct raw curl commands.
**Fix:** Create a bash SDK that agents source:
```bash
#!/usr/bin/env bash
# Stronghold Agent SDK
# Usage: source stronghold-agent.sh

STRONGHOLD_URL="${STRONGHOLD_URL:?Set STRONGHOLD_URL}"
STRONGHOLD_TOKEN="${STRONGHOLD_TOKEN:?Set STRONGHOLD_TOKEN}"

# Create a task and get a machine
stronghold_task() {
  local instruction="$1" image="${2:-alpine}" ttl="${3:-3600}"
  curl -sk -X POST "$STRONGHOLD_URL/agent/task" \
    -H "Authorization: Bearer $STRONGHOLD_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"instruction\":\"$instruction\",\"image\":\"$image\",\"ttl_secs\":$ttl}"
}

# Execute a command on a machine
stronghold_exec() {
  local machine_id="$1" cmd="$2"
  curl -sk -X POST "$STRONGHOLD_URL/agent/$machine_id/exec" \
    -H "Authorization: Bearer $STRONGHOLD_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"cmd\":\"$cmd\",\"timeout_secs\":300}"
}

# Clone a repo
stronghold_git_clone() {
  local machine_id="$1" repo="$2"
  curl -sk -X POST "$STRONGHOLD_URL/agent/$machine_id/git/clone" \
    -H "Authorization: Bearer $STRONGHOLD_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"repo\":\"$repo\"}"
}

# Create a branch
stronghold_git_branch() { ... }
# Commit
stronghold_git_commit() { ... }
# Push
stronghold_git_push() { ... }
# Create PR
stronghold_git_pr() { ... }
# Get credential
stronghold_credential() {
  local machine_id="$1" name="$2"
  curl -sk "$STRONGHOLD_URL/agent/$machine_id/credentials/$name" \
    -H "Authorization: Bearer $STRONGHOLD_TOKEN"
}
# Mid-session reprompt
stronghold_instruct() {
  local machine_id="$1" instruction="$2" mode="${3:-pty}"
  curl -sk -X POST "$STRONGHOLD_URL/agent/$machine_id/instruct" \
    -H "Authorization: Bearer $STRONGHOLD_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"instruction\":\"$instruction\",\"mode\":\"$mode\"}"
}
# Submit task result
stronghold_result() {
  local task_id="$1" exit_code="$2" summary="$3"
  curl -sk -X POST "$STRONGHOLD_URL/agent/task/$task_id/result" \
    -H "Authorization: Bearer $STRONGHOLD_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"exit_code\":$exit_code,\"summary\":\"$summary\"}"
}
# Open PTY
stronghold_shell() {
  local machine_id="$1" token="$2"
  websocat "wss://${STRONGHOLD_URL#https://}/agent/$machine_id/pty?token=$token"
}
```

**DoD:** `source stronghold-agent.sh && stronghold_task "test" "alpine" 300` returns valid JSON. All functions produce correct curl commands.
**Test:** Shellcheck the script. Verify curl commands match the API.
**Context budget:** ~150 lines in new file, ~50 lines in README.

### N2: Shared workspace for multi-agent collaboration (subagent)

**Files:** `gateway/src/machines/scheduler.rs` (only)
**Current:** Each pod gets its own `emptyDir`. No shared workspace.
**Fix:**
1. Add a `project_id` concept — when creating a pod, if the task specifies a `project_id`, mount a shared RWX PVC:
   - PVC name: `project-<project_id>-work`
   - Access mode: `ReadWriteMany`
   - Storage class: `nfs` or `longhorn` (k3s default `local-path` doesn't support RWX — document this)
   - Mounted at: `/home/dev/work`
2. If no `project_id`, use the existing per-pod `emptyDir` (backward compat)
3. The `OrderRequest` (or Task spec) gains an optional `project_id` field
4. Multiple tasks with the same `project_id` share the same volume

**DoD:** Create two tasks with the same `project_id`. Both pods can read/write files in `/home/dev/work`. File written by pod A is visible in pod B.
**Test:** Unit test the PVC name generation and volume mount logic.
**Context budget:** ~60 lines in scheduler.rs.

### N3: Agent-to-agent message bus (subagent)

**Files:** `gateway/src/routes/messages.rs` (new), `gateway/src/routes/mod.rs` (add routes)
**Current:** No inter-agent communication.
**Fix:**
1. Simple message bus using SQLite (no Redis dependency):
   - `POST /agent/:machine_id/messages` — post a message:
     - Request: `{ to: Option<String>, channel: String, body: serde_json::Value }`
     - `to` is a specific machine_id (DM) or null (broadcast to channel)
     - Stores in a new `agent_messages` table
   - `GET /agent/:machine_id/messages?channel=<ch>&since=<ts>` — poll for messages
     - Returns messages addressed to this machine or broadcast to the channel
   - `GET /agent/:machine_id/messages/stream` — SSE stream of messages (polls DB every 500ms)
2. Add `agent_messages` table to schema.sql:
```sql
CREATE TABLE IF NOT EXISTS agent_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    from_machine TEXT NOT NULL,
    to_machine   TEXT,
    channel      TEXT NOT NULL,
    body         TEXT NOT NULL,
    created_at   TEXT NOT NULL
);
```
3. Register routes in `mod.rs`

**DoD:** Agent A posts a message to channel "build-status". Agent B's SSE stream receives it within 1 second.
**Test:** Unit test message posting and retrieval.
**Context budget:** ~120 lines in new file, ~15 lines in schema.sql, 3 lines in mod.rs.

### N4: Control WebSocket channel (orchestrator-only — core feature)

**Files:** `gateway/src/routes/instruct.rs` (extend J4), `gateway/src/routes/mod.rs` (add route)
**Current:** J4 added the instruct endpoint but the control WebSocket channel was specified but not implemented.
**Fix:**
1. `GET /agent/:machine_id/control` — WebSocket control channel:
   - Verifies connect_token (same as PTY)
   - Opens a WebSocket that delivers JSON messages to the agent's runtime:
     - `{"type":"instruct","instruction":"...","context":{...},"priority":"high"}`
     - `{"type":"shutdown","reason":"gateway_restarting"}`
     - `{"type":"quorum_required","cmd":"...","scope":"destructive"}`
     - `{"type":"task_update","task_id":"...","status":"completed"}`
   - The agent's runtime listens on this channel alongside the PTY
2. When `POST /agent/:machine_id/instruct` is called with `mode: "control"`:
   - Look up the control channel sender in the pty_registry (or a separate control_registry)
   - Send the JSON message on the WebSocket
3. Register route: `.route("/agent/:machine_id/control", axum::routing::get(instruct::control_ws))`

**DoD:** Agent opens both PTY and control WebSockets. `POST /instruct` with `mode: "control"` delivers a JSON message on the control WebSocket. Agent receives it.
**Test:** Unit test the control message format. Integration test with mock WebSocket.
**Context budget:** ~100 lines in instruct.rs, 1 line in mod.rs.

**Wave N DoD:**
- [ ] Agent bash SDK works — all functions produce valid API calls
- [ ] Shared workspace (RWX PVC) allows multi-agent file sharing
- [ ] Agent-to-agent message bus works (post + poll + SSE)
- [ ] Control WebSocket delivers structured JSON messages to the agent runtime
- [ ] `POST /instruct` with `mode: "control"` delivers messages on the control channel
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave O — Observability & Polish (4 tasks)

**Goal:** Real-time monitoring, documentation, end-to-end tests.

### O1: Structured task monitoring on phone (subagent)

**Files:** `phone/enroll.html` (only)
**Current:** Phone shows raw PTY (if it worked) and session cards. No structured task view.
**Fix:**
1. Add a "Tasks" section to the phone PWA:
   - Subscribes to `GET /agent/task/:id/stream` SSE for each active task
   - Shows: task instruction, status (queued/running/completed/failed), duration, exit code
   - Shows: latest command executed (from audit stream), stdout snippet
   - Shows: step progress if the task is part of a workflow (step 2 of 4)
2. Add a "Workflows" section:
   - Shows: workflow DAG as a simple list with status indicators (✅/⏳/❌)
   - Subscribes to `GET /workflow/run/:id/stream` SSE
3. Uses the existing SSE infrastructure (fetch-based streaming reader)

**DoD:** Phone shows structured task progress (not raw PTY). Workflow steps show as a checklist with real-time status.
**Test:** Verify HTML/JS is valid. Verify SSE event parsing.
**Context budget:** ~200 lines in enroll.html.

### O2: End-to-end integration test (subagent)

**Files:** `gateway/tests/e2e_test.rs` (new)
**Current:** No end-to-end test of the task → exec → git → result flow.
**Fix:** Write an integration test that:
1. Creates a tenant + quota + agent token
2. Creates a task: `POST /agent/task` with `{"instruction":"echo hello","image":"alpine","ttl_secs":300}`
3. Verifies task status is `queued` then `running`
4. Executes a command: `POST /agent/:machine_id/exec` with `{"cmd":"echo","args":["test"]}`
5. Verifies structured output: `exit_code: 0, stdout: "test\n"`
6. Submits task result: `POST /agent/task/:id/result` with `{"exit_code":0,"summary":"done"}`
7. Verifies task status is `completed`
8. Verifies audit entries exist for: task_created, cmd_exec, task_completed
9. All using in-memory DB (no real k3s needed — mock the exec)

**DoD:** Test passes. Full task lifecycle verified end-to-end.
**Test:** The test IS the test.
**Context budget:** ~200 lines in new file.

### O3: Documentation rewrite — Stronghold is an orchestrator (subagent)

**Files:** `README.md`, `CHANGELOG.md`, `docs/PROTOCOL.md`, `docs/OPERATIONS.md`
**Current:** Docs describe Stronghold as a "gateway" — a secure shell provisioner.
**Fix:** Rewrite docs to describe Stronghold as an **agent orchestration platform**:
1. `README.md`: Update the description, architecture diagram, quick start
2. `CHANGELOG.md`: Add `[1.0.0-rc]` section with all new features
3. `docs/PROTOCOL.md`: Document all new endpoints (task, exec, instruct, git, credentials, workflow, messages, control)
4. `docs/OPERATIONS.md`: Document task management, workflow management, credential management

**DoD:** Docs accurately describe the orchestrator capabilities. No references to "just a gateway" or "secure shell provisioner."
**Test:** Manual review — every endpoint in the code has corresponding doc.
**Context budget:** ~500 lines across 4 files.

### O4: Final quality gate + tag (orchestrator-only)

**Files:** All
**Current:** N/A
**Fix:**
1. Run full quality gate: `cargo build && cargo clippy -- -D warnings && cargo test`
2. Run `cargo fmt --all -- --check`
3. Update `CHANGELOG.md` with final version
4. Tag `v1.0.0-rc`
5. Append final worklog entry

**DoD:** All tests pass. All clippy warnings clean. Format clean. Tag pushed.
**Test:** The gate IS the test.

**Wave O DoD:**
- [ ] Phone PWA shows structured task progress (not just raw PTY)
- [ ] Workflow steps show as a real-time checklist
- [ ] End-to-end integration test passes (task → exec → result → audit)
- [ ] All docs rewritten to reflect orchestrator capabilities
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean, `cargo fmt --check` clean
- [ ] Tag `v1.0.0-rc` pushed to GitHub

---

## Subagent Prompt Template

```
Task ID: <ID>

You are implementing ONE feature in the Stronghold project.

FILE SCOPE: You may ONLY modify these files:
- <file 1>
- <file 2 if needed>
Do NOT touch any other files.

CURRENT STATE: <what the code does now>

FIX: <precise description of what to build>

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
Wave J (5 tasks, 2 orchestrator + 3 subagent):
  J1 (orchestrator, DB schema) → J2,J3 parallel → J4 (orchestrator, instruct) → J5 → gate → push

Wave K (4 tasks, 1 orchestrator + 3 subagent):
  K1 (orchestrator, crypto vault) → K2,K3 parallel → K4 → gate → push

Wave L (5 tasks, all subagent):
  L1 → L2,L3 parallel → L4,L5 parallel → gate → push

Wave M (4 tasks, 1 orchestrator + 3 subagent):
  M1 → M2 (orchestrator, engine) → M3,M4 parallel → gate → push

Wave N (4 tasks, 1 orchestrator + 3 subagent):
  N1,N2,N3 parallel → N4 (orchestrator, control WS) → gate → push

Wave O (4 tasks, 1 orchestrator + 3 subagent):
  O1,O2,O3 parallel → O4 (orchestrator, final gate + tag) → gate → push → DONE
```

Total: 26 tasks across 5 waves. Orchestrator must not return until all wave DoDs pass.

---

## DoD Loop Protocol

```
function execute_wave(wave):
    for task in wave.tasks:
        if task.orchestrator_only:
            implement(task)
        else:
            subagent = spawn(task)
            result = await(subagent)
            if result.failed:
                fix_or_rebrief(task)
        commit(task)
    
    push()
    sync_dev_box()
    
    while not wave_dod_passes(wave):
        failing_check = identify_failure(wave)
        if failing_check.is_code_issue:
            fix(failing_check)
        elif failing_check.is_missing_feature:
            implement(failing_check)
        else:
            re_brief_subagent(failing_check)
        commit(fix)
        push()
        sync_dev_box()
    
    return SUCCESS

# Main loop — MUST NOT RETURN UNTIL ALL WAVES PASS
for wave in [J, K, L, M, N, O]:
    execute_wave(wave)

# All waves passed — tag the release
tag("v1.0.0-rc")
```

The orchestrator agent MUST NOT return until `tag("v1.0.0-rc")` is executed. If any wave DoD fails, the orchestrator loops on that wave until it passes. No partial completion is acceptable.
