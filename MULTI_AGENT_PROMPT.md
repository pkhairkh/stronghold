# Stronghold — Multi-Agent System Implementation Prompt

> 5 waves, 22 surgical tasks. Implements the full multi-agent agentic coding system: watchdogs, role assignment, communication protocols, workflow engine extensions, and reflexion loops.
>
> **The orchestrator agent MUST NOT return until ALL wave DoDs pass.**

---

## 0. Orchestrator Protocol

### Execution loop
```
for each wave P..T:
    1. READ worklog.md + this prompt
    2. EXECUTE: spawn subagents (max 4 parallel), do own tasks serially
    3. REVIEW: read every changed file, run build+clippy+test
    4. COMMIT: one commit per task
    5. PUSH: push after all tasks pass
    6. GATE: run wave DoD — if ANY check fails, fix and re-gate
    7. NEXT WAVE
```

### Hard rules
- One task per subagent. Each gets: task ID, file scope (1-3 files), fix, DoD, tests.
- Orchestrator does NOT delegate: watchdog monitoring loop, workflow engine changes, DB schema.
- After each task: `cargo build && cargo clippy -- -D warnings && cargo test`
- After each wave: push + sync dev box + run wave DoD.
- **Orchestrator MUST NOT return until ALL wave DoDs pass.**

### Dev box + Git
```bash
python3 /home/z/my-project/scripts/ssh_exec.py '<command>'
cd /home/z/my-project/stronghold
git add <files> && git commit -m "<task-id>: <summary>"
git push origin main
```

---

## Wave P — Watchdog System (5 tasks)

**Goal:** Real-time agent monitoring with dedication scoring, ultimatum protocol, and workaround detection.

### P1: Watchdog DB schema (orchestrator-only)

**Files:** `gateway/src/db/schema.sql`, `gateway/src/db/mod.rs` (migration 004)
**Fix:** Add tables:
```sql
CREATE TABLE IF NOT EXISTS watchdog_reports (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    watcher_machine TEXT NOT NULL,
    watched_machine TEXT NOT NULL,
    watched_task_id TEXT,
    dedication_score REAL NOT NULL,
    progress_files  INTEGER,
    progress_tests  INTEGER,
    progress_commits INTEGER,
    last_activity_secs INTEGER,
    workaround_warnings TEXT,   -- JSON array
    ultimatum_level INTEGER DEFAULT 0,
    assessment      TEXT,
    created_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ultimata (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    watchdog_machine TEXT NOT NULL,
    target_machine  TEXT NOT NULL,
    target_task_id  TEXT,
    level           INTEGER NOT NULL,   -- 1, 2, or 3
    message         TEXT NOT NULL,
    acknowledged    INTEGER DEFAULT 0,
    created_at      TEXT NOT NULL,
    acknowledged_at TEXT
);
```
Add migration 004 to mod.rs (same pattern as 003 — check _migrations table, CREATE TABLE IF NOT EXISTS).
**DoD:** Both tables exist. Migration is idempotent. Test passes.
**Test:** `test_watchdog_tables_exist`

### P2: Dedication scoring engine (subagent)

**Files:** `gateway/src/watchdog/dedication.rs` (new), `gateway/src/watchdog/mod.rs` (new)
**Fix:** Create the dedication scoring engine:
```rust
pub struct DedicationScore {
    pub score: f64,              // 0.0 - 1.0
    pub relevant_commands: usize,
    pub total_commands: usize,
    pub progress_rate: f64,
    pub task_alignment: f64,
}

pub fn compute_dedication(
    recent_audit_entries: &[AuditEntry],
    task_keywords: &[String],
    progress_indicators: &ProgressIndicators,
) -> DedicationScore
```
- `relevant_commands`: count audit entries where `cmd` contains any task keyword
- `total_commands`: total audit entries in the window
- `progress_rate`: `min(1.0, (files_changed + tests_run + commits) as f64 / 5.0)`
- `task_alignment`: 1.0 if recent output contains task keywords, 0.5 if neutral, 0.0 if unrelated
- `score`: `(relevant / max(total, 1)) * progress_rate * task_alignment`

Also add `ProgressIndicators` struct: `{ files_changed, tests_run, commits, last_activity_secs }` with a `from_audit_entries` constructor that parses audit entries for file writes, test runs, and git commits.

**DoD:** `compute_dedication` returns correct scores for test inputs. Edge cases (empty audit, all relevant, none relevant) handled.
**Test:** 5+ unit tests: empty input, all-relevant, none-relevant, partial, high-progress.

### P3: Workaround detector (subagent)

**Files:** `gateway/src/watchdog/detector.rs` (new, same module)
**Fix:** Create the workaround detector:
```rust
pub struct WorkaroundWarning {
    pub pattern: String,
    pub severity: String,   // "critical", "high", "medium", "low"
    pub file: Option<String>,
    pub line: Option<u32>,
    pub message: String,
}

pub fn detect_workarounds(
    recent_output: &str,
    git_diff: &str,
) -> Vec<WorkaroundWarning>
```
Check for these patterns in `recent_output` (PTY output) and `git_diff` (staged changes):
- `unwrap()` or `expect(` in new code → high
- `#[allow(dead_code)]` or `#[allow(clippy:` → high
- `#[ignore]` on tests → high
- `todo!()` or `unimplemented!()` → critical
- `// TODO` or `// FIXME` in new lines (lines starting with `+`) → medium
- `println!` or `dbg!` in new lines → medium
- `unimplemented!()` → critical
- Same command appearing 3+ times in recent_output → spin (high)

**DoD:** Detector catches all 8 pattern types. No false positives on clean code.
**Test:** 8+ unit tests, one per pattern type + a clean-code negative test.

### P4: Ultimatum injection (subagent)

**Files:** `gateway/src/watchdog/ultimatum.rs` (new, same module)
**Fix:** Create the ultimatum system:
```rust
pub enum UltimatumLevel {
    Warning,    // Level 1
    Directive,  // Level 2
    Escalation, // Level 3
}

pub struct Ultimatum {
    pub level: UltimatumLevel,
    pub target_machine: String,
    pub target_task_id: Option<String>,
    pub message: String,
    pub deadline_seconds: Option<u64>,
}

pub async fn issue_ultimatum(
    state: &AppState,
    ultimatum: &Ultimatum,
) -> Result<()>
```
- Level 1: inject via `state.pty_registry` (same as instruct mode=control) with message
- Level 2: inject + store in `ultimata` table + set `acknowledged=0`
- Level 3: inject + store + post `escalation` on message bus + call `push_anomaly` for phone notification
- Add `check_ultimatum_acknowledgment(db, ultimatum_id) -> bool` that checks if the agent ran `echo ACK_TASK_FOCUS` (detected via audit stream)

**DoD:** Ultimatum can be issued at all 3 levels. DB entry created. Phone notified at Level 3.
**Test:** 3 unit tests (one per level) + serialization tests.

### P5: Watchdog monitoring loop (orchestrator-only)

**Files:** `gateway/src/watchdog/monitor.rs` (new), `gateway/src/main.rs` (add background task)
**Fix:** Create the monitoring loop:
```rust
pub async fn start_watchdog_loop(state: AppState) {
    loop {
        // 1. Query all active machines
        // 2. For each machine with an active task:
        //    a. Fetch recent audit entries (last 5 minutes)
        //    b. Compute dedication score
        //    c. Detect workarounds
        //    d. If dedication < 0.3 for 3 checks: issue Level 1
        //    e. If Level 1 issued and still low after 2 min: Level 2
        //    f. If Level 2 not acknowledged after 2 min: Level 3
        //    g. Store watchdog_report in DB
        // 3. Sleep 60 seconds
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
```
In `main.rs::serve()`, spawn: `tokio::spawn(watchdog::monitor::start_watchdog_loop(state.clone()));`

**DoD:** Watchdog loop runs every 60s. Stores reports in DB. Issues ultimata when dedication is low.
**Test:** Unit test the monitoring logic with mock audit entries.

**Wave P DoD:**
- [ ] `watchdog_reports` and `ultimata` tables exist
- [ ] `compute_dedication()` returns correct scores
- [ ] `detect_workarounds()` catches all 8 pattern types
- [ ] `issue_ultimatum()` works at all 3 levels
- [ ] Watchdog loop runs as background task in `serve()`
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave Q — Agent Roles & Constitutional Principles (4 tasks)

**Goal:** Role assignment, system prompt injection, constitutional preamble.

### Q1: Agent roles DB schema + API (orchestrator-only)

**Files:** `gateway/src/db/schema.sql`, `gateway/src/db/mod.rs` (migration 005)
**Fix:** Add:
```sql
CREATE TABLE IF NOT EXISTS agent_roles (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    name            TEXT NOT NULL,       -- "planner", "coder", "reviewer", etc.
    system_prompt   TEXT NOT NULL,       -- full system prompt text
    allowed_tools   TEXT NOT NULL,       -- JSON array: ["exec", "git/clone", ...]
    denied_tools    TEXT NOT NULL,       -- JSON array: ["git/push", "git/pr", ...]
    created_at      TEXT NOT NULL,
    UNIQUE(tenant_id, name)
);
```
Add `POST /admin/roles` (create), `GET /admin/roles?tenant=<id>` (list), `GET /admin/roles/:id` (get) to `routes/admin.rs`.
Seed default roles (planner, coder, reviewer, tester, integrator, watchdog, oracle, architect, facilitator) with prompts from `agent/prompts/*.md` on first tenant creation.

**DoD:** Roles can be created, listed, retrieved. Default roles seeded.
**Test:** Create role, list roles, verify fields.

### Q2: Constitutional principles endpoint (subagent)

**Files:** `gateway/src/routes/admin.rs` (extend)
**Fix:** Add:
- `GET /admin/constitution` — returns the 10 constitutional principles as JSON
- `POST /admin/constitution` — update principles (requires admin auth)
- Store in a `constitution` table or as a JSON file at `/var/lib/stronghold/constitution.json`
- The constitution is prepended to every agent's system prompt at session start

**DoD:** Constitution can be retrieved and updated. Default 10 principles exist.
**Test:** Get constitution returns 10 principles. Update changes the response.

### Q3: Role assignment in task creation (subagent)

**Files:** `gateway/src/routes/tasks.rs` (extend)
**Fix:** Add optional `role` field to `CreateTaskRequest`:
```rust
pub role: Option<String>,  // "planner", "coder", "reviewer", etc.
```
When a task is created with a role:
1. Look up the role in `agent_roles` table
2. Store the `system_prompt` in the task's `spec` JSON
3. The `allowed_tools`/`denied_tools` are enforced by the exec/git endpoints (they check the task's role before allowing the operation)

Add `role` column to `tasks` table (migration 005, same as Q1).

**DoD:** Task with `role: "coder"` stores the coder system prompt in spec. Task with `role: "reviewer"` stores the reviewer prompt.
**Test:** Create tasks with different roles, verify spec contains the correct prompt.

### Q4: Tool enforcement based on role (subagent)

**Files:** `gateway/src/routes/exec.rs` (extend), `gateway/src/routes/git.rs` (extend)
**Fix:** Before executing a command or git operation:
1. Look up the machine's current task and its role
2. Check the role's `allowed_tools` and `denied_tools`
3. If the requested tool is in `denied_tools`, return 403
4. If `allowed_tools` is non-empty and the tool is not in it, return 403

Example: a Reviewer role has `denied_tools: ["git/push", "git/pr", "git/commit"]` — the Coder endpoints return 403 for a Reviewer's machine.

**DoD:** Reviewer cannot push. Coder cannot merge. Watchdog cannot exec at all.
**Test:** Mock role with denied tools, verify 403 response.

**Wave Q DoD:**
- [ ] Agent roles can be created, listed, retrieved
- [ ] Default 9 roles seeded on tenant creation
- [ ] Constitutional principles endpoint works
- [ ] Tasks can be assigned a role with system prompt injection
- [ ] Tool enforcement blocks denied operations per role
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave R — Communication & Coordination (4 tasks)

**Goal:** Oracle Q&A, Facilitator decisions, progress reports, help requests.

### R1: Oracle Q&A endpoint (subagent)

**Files:** `gateway/src/routes/oracle.rs` (new), `gateway/src/routes/mod.rs` (add route)
**Fix:** Add `POST /agent/:machine_id/oracle` — ask the Oracle a question:
- Request: `{ question: String, context: Option<serde_json::Value> }`
- Verifies agent token
- Stores the question in `agent_messages` (channel: `oracle-<machine_id>`)
- Returns: `{ question_id: String, status: "queued" }`
- The Oracle agent (running in its own session) polls for questions on its channel, answers them, and posts the answer back

Also add `GET /agent/:machine_id/oracle/:question_id` — get the Oracle's answer (polls until answered or timeout).

**DoD:** Agent can ask a question. Answer can be retrieved.
**Test:** Post question, verify it's stored. Get answer (mock).

### R2: Facilitator disagreement endpoint (subagent)

**Files:** `gateway/src/routes/facilitator.rs` (new), `gateway/src/routes/mod.rs` (add route)
**Fix:** Add:
- `POST /agent/:machine_id/disagreement` — submit a disagreement:
  - Request: `{ task_id, issue, coder_argument, reviewer_argument, context }`
  - Stores in a new `disagreements` table (add to schema.sql + migration)
  - Posts on `workflow-run-<run_id>` channel for the Facilitator
  - Returns: `{ disagreement_id, status: "pending" }`

- `GET /agent/:machine_id/disagreement/:id` — get the Facilitator's decision:
  - Returns: `{ decision, reasoning, precedent, binding: true }`
  - Polls until decision is made or timeout

**DoD:** Disagreement can be submitted. Decision can be retrieved.
**Test:** Submit disagreement, verify stored. Get decision (mock).

### R3: Progress report endpoint (subagent)

**Files:** `gateway/src/routes/tasks.rs` (extend)
**Fix:** Add `POST /agent/task/:id/progress` — agent submits a progress report:
- Request: `{ files_changed: Vec<String>, tests_run: u32, tests_passing: u32, commits: u32, blockers: Vec<String>, status: String }`
- Stores in `task_outputs` as key `progress_<timestamp>`
- Posts `progress` message on `workflow-run-<run_id>` channel
- The phone PWA can subscribe to task progress via the existing SSE stream

**DoD:** Agent can submit progress. Progress is stored and broadcast.
**Test:** Submit progress, verify stored in task_outputs.

### R4: Reflexion storage and retrieval (subagent)

**Files:** `gateway/src/routes/tasks.rs` (extend)
**Fix:** Add:
- `POST /agent/task/:id/reflexion` — agent submits post-task reflexion:
  - Request: `{ what_went_well: String, what_went_wrong: String, what_differently: String, what_learned: String }`
  - Stores in `task_outputs` as key `reflexion`
  - The Planner can query past reflexions when planning similar tasks

- `GET /agent/task/:id/reflexion` — get the reflexion for a task
- `GET /agent/reflexions?tenant=<id>&limit=10` — list recent reflexions (for the Planner to learn from)

**DoD:** Reflexion can be submitted and retrieved. List endpoint works.
**Test:** Submit reflexion, retrieve it, verify fields. List returns multiple.

**Wave R DoD:**
- [ ] Oracle Q&A endpoint works (post question, get answer)
- [ ] Facilitator disagreement endpoint works
- [ ] Progress report endpoint works
- [ ] Reflexion storage and retrieval works
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave S — Workflow Engine Extensions (5 tasks)

**Goal:** Parallel execution, re-planning, template registry, strategy support.

### S1: `parallel_with` support in DAG executor (orchestrator-only)

**Files:** `gateway/src/workflow/engine.rs` (extend)
**Current:** Steps only support `depends_on` (sequential). No `parallel_with`.
**Fix:**
1. Add `parallel_with: Option<String>` to the step JSON schema
2. In the DAG executor: a step with `parallel_with: "other_step"` starts at the same time as `other_step` (when `other_step`'s dependencies are met)
3. Both steps run concurrently via `tokio::spawn`
4. Downstream steps that `depends_on` either of the parallel steps wait for BOTH to complete

**DoD:** Two steps with `parallel_with` start simultaneously. Downstream waits for both.
**Test:** Unit test with a 3-step DAG: A → (B parallel_with A) → C depends_on [A, B]. Verify B starts at same time as A, C waits for both.

### S2: Re-planning on failure (orchestrator-only)

**Files:** `gateway/src/workflow/engine.rs` (extend)
**Current:** Failed steps retry up to `max_retries`, then the workflow fails.
**Fix:** After `max_retries` exhausted:
1. Post `replan_needed` on the message bus with the failure context
2. Check if the workflow has a `replan_strategy` field:
   - `"abort"` (default): mark workflow as failed, notify human
   - `"auto_adjust"`: the Planner agent receives the failure and creates a modified DAG. The new DAG replaces the remaining steps. Execution continues with the new plan.
   - `"human_approval"`: pause workflow, send phone push, wait for human to approve the new plan
3. Store the original plan and the revised plan in `workflow_runs.result` for audit

**DoD:** Failed workflow with `replan_strategy: "auto_adjust"` triggers re-planning. Workflow with `"abort"` fails cleanly.
**Test:** Mock failure, verify replan is triggered. Mock abort, verify clean failure.

### S3: Workflow template registry (subagent)

**Files:** `gateway/src/routes/workflows.rs` (extend)
**Fix:** Add:
- `GET /workflow/templates` — list all available templates (reads from `agent/templates/*.json` on disk, or from a `workflow_templates` table)
- `POST /workflow/templates` — register a new template (stores in DB)
- `GET /workflow/templates/:name` — get a template by name
- `POST /workflow/templates/:name/instantiate` — create a workflow from a template (replaces placeholders in the DAG with actual values)

Add `workflow_templates` table:
```sql
CREATE TABLE IF NOT EXISTS workflow_templates (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    name        TEXT NOT NULL,
    dag         TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    UNIQUE(tenant_id, name)
);
```
Seed the 12 built-in templates on first tenant creation.

**DoD:** Templates can be listed, retrieved, and used to create workflows.
**Test:** List templates returns 12. Instantiate creates a workflow.

### S4: Conditional expression evaluator (subagent)

**Files:** `gateway/src/workflow/engine.rs` (extend)
**Current:** Conditions support `step_id.result.exit_code == 0` only.
**Fix:** Extend the condition evaluator to support:
- `step_id.result.exit_code == 0` (integer comparison)
- `step_id.result.failed == 0` (field access)
- `step_id.result.passed > 0` (greater than)
- `step_id.result.approved == true` (boolean)
- `step_id.result.approved != false` (negation)
- `step_id.result.lint == "clean"` (string comparison)
- `step_id.result.skipped == true` (skipped step)
- `!step_id.result.failed > 0` (NOT + comparison)
- Compound: `step_a.result.exit_code == 0 && step_b.result.passed > 0` (AND)
- Compound: `step_a.result.exit_code == 0 || step_b.result.exit_code == 0` (OR)

Parse with a simple recursive descent parser. No external crate needed.

**DoD:** All 10 condition types evaluate correctly.
**Test:** 10+ unit tests, one per condition type + compound tests.

### S5: Workflow run SSE with step-level detail (subagent)

**Files:** `gateway/src/routes/workflows.rs` (extend)
**Fix:** Add `GET /workflow/run/:id/stream` — SSE stream:
- Emits `step_started`, `step_completed`, `step_failed`, `step_skipped` events
- Each event includes: step_id, task_id, status, result summary
- Emits `workflow_completed` or `workflow_failed` on terminal state
- 30s heartbeat
- Same pattern as `tasks::stream_task`

**DoD:** SSE stream emits step-level events in real-time.
**Test:** Mock workflow run, verify SSE events.

**Wave S DoD:**
- [ ] `parallel_with` works — concurrent step execution
- [ ] Re-planning triggers on failure (3 strategies: abort, auto_adjust, human_approval)
- [ ] Template registry — 12 templates listed, instantiable
- [ ] Conditional expressions support all 10 types + compound AND/OR
- [ ] Workflow run SSE stream emits step-level events
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Wave T — Integration & Testing (4 tasks)

**Goal:** End-to-end multi-agent test, template validation, documentation.

### T1: Multi-agent E2E integration test (subagent)

**Files:** `gateway/tests/multi_agent_test.rs` (new)
**Fix:** Write a test that:
1. Creates a tenant + 9 agent roles (seeded)
2. Creates a credential (github-pat)
3. Creates a workflow from the `standard-cicd` template
4. Starts the workflow run
5. Simulates each step:
   - Plan task: create, set status running, submit result with plan
   - Implement task: create with role=coder, submit result with exit_code=0
   - Test task: create with role=tester, submit result with passed=10, failed=0
   - Review task: create with role=reviewer, submit result with approved=true
   - Merge task: create with role=integrator, submit result with merged=true
6. Verifies workflow run status is "completed"
7. Verifies all 5 tasks exist with correct statuses
8. Verifies audit entries exist for each task lifecycle event
9. Submits a reflexion for one task and verifies it's retrievable
10. All in-memory DB, no real k3s

**DoD:** Test passes. Full multi-agent lifecycle verified.
**Test:** The test IS the test.

### T2: Template validation test (subagent)

**Files:** `gateway/tests/template_test.rs` (new)
**Fix:** Write a test that:
1. Reads all 12 template JSON files from `agent/templates/`
2. For each template:
   - Verifies the JSON is valid
   - Verifies it has `name` and `dag.steps`
   - Verifies every step has `id`, `task.instruction`, `task.image`, `task.ttl_secs`
   - Verifies `depends_on` references valid step IDs (no dangling deps)
   - Verifies no circular dependencies
   - Verifies `condition` references valid step IDs
   - Verifies `parallel_with` references valid step IDs
3. Asserts all 12 templates pass validation

**DoD:** All 12 templates are valid. No dangling refs, no cycles.
**Test:** The test IS the test.

### T3: Watchdog integration test (subagent)

**Files:** `gateway/tests/watchdog_test.rs` (new)
**Fix:** Write a test that:
1. Creates a tenant + task + machine
2. Inserts mock audit entries simulating an off-task agent (commands unrelated to task keywords)
3. Calls `compute_dedication()` with the mock entries
4. Asserts dedication score < 0.3
5. Calls `detect_workarounds()` with output containing `unwrap()` and `#[allow(clippy::`
6. Asserts 2+ workaround warnings detected
7. Calls `issue_ultimatum()` at Level 1
8. Verifies ultimatum is stored in DB
9. Inserts mock audit entry with `echo ACK_TASK_FOCUS`
10. Calls `check_ultimatum_acknowledgment()` — asserts true

**DoD:** Full watchdog cycle tested: detect low dedication → detect workarounds → issue ultimatum → verify acknowledgment.
**Test:** The test IS the test.

### T4: Documentation update (subagent)

**Files:** `docs/AGENT_SYSTEM.md` (new), `README.md` (update), `CHANGELOG.md` (update)
**Fix:**
1. Create `docs/AGENT_SYSTEM.md` — comprehensive documentation of the multi-agent system:
   - Architecture overview with ASCII diagram
   - All 9 agent roles with responsibilities
   - All 5 team strategies with when-to-use
   - Watchdog system (dedication scoring, ultimatum protocol, workaround detection)
   - Communication protocols (Q&A, progress, help, disagreement, watchdog report)
   - Constitutional principles
   - Reflexion loops
   - Re-planning protocol
   - All 12 workflow templates with descriptions
   - API reference for all new endpoints
2. Update `README.md` — add "Multi-Agent System" section
3. Update `CHANGELOG.md` — add new section with all multi-agent features

**DoD:** Docs accurately describe the system. Every endpoint has documentation.
**Test:** Manual review.

**Wave T DoD:**
- [ ] Multi-agent E2E test passes (workflow → 5 tasks → reflexion)
- [ ] All 12 templates validate (no dangling refs, no cycles)
- [ ] Watchdog integration test passes (dedication → workarounds → ultimatum → ack)
- [ ] `docs/AGENT_SYSTEM.md` exists and is comprehensive
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean
- [ ] Pushed to GitHub

---

## Subagent Prompt Template

```
Task ID: <ID>

You are implementing ONE feature in the Stronghold project.

FILE SCOPE: You may ONLY modify these files:
- <file 1>
- <file 2 if needed>

CURRENT STATE: <what exists now>

FIX: <precise description>

CONSTRAINTS:
- Do NOT touch files outside the scope.
- Run: cd /root/stronghold && cargo build --workspace --features no-sev-snp
- Run: cd /root/stronghold && cargo clippy --workspace --features no-sev-snp -- -D warnings
- Run: cd /root/stronghold && cargo test --workspace --features no-sev-snp
- All three must pass.
- Commit: git add <files> && git commit -m "<ID>: <summary>"
- Push: git push origin main

DOD: <what "done" looks like>
TESTS: <what tests to write>

Return: files changed, test count, any issues.
```

---

## Execution Order

```
Wave P (5 tasks, 2 orchestrator + 3 subagent):
  P1 (orchestrator, DB) → P2,P3,P4 parallel → P5 (orchestrator, loop) → gate → push

Wave Q (4 tasks, 1 orchestrator + 3 subagent):
  Q1 (orchestrator, schema+API) → Q2,Q3,Q4 parallel → gate → push

Wave R (4 tasks, all subagent):
  R1,R2,R3,R4 parallel → gate → push

Wave S (5 tasks, 2 orchestrator + 3 subagent):
  S1 (orchestrator, engine) → S2 (orchestrator, engine) → S3,S4,S5 parallel → gate → push

Wave T (4 tasks, all subagent):
  T1,T2,T3,T4 parallel → gate → push

Final: tag v1.1.0
```

Total: 22 tasks across 5 waves.

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
        fix(failing_check)
        commit(fix)
        push()
        sync_dev_box()

    return SUCCESS

# Main loop — MUST NOT RETURN UNTIL ALL WAVES PASS
for wave in [P, Q, R, S, T]:
    execute_wave(wave)

tag("v1.1.0")
```

The orchestrator agent MUST NOT return until `tag("v1.1.0")` is executed.
