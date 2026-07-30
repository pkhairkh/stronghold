# CONTINUOUS AGENT ORCHESTRATION FIX — Waves CA through CF

> **Orchestrator contract:** You must not return until every wave's DoD
> passes. Per-task commits, per-wave pushes, per-wave DoD gates. If any
> DoD fails, diagnose → patch → re-test → re-commit until green.
>
> **Context:** The live agent test exposed 5 critical bugs. Agents connect
> but share the same pod, work on the same task without coordination,
> never report results, the watchdog spams noise, and nothing lands on
> GitHub. This prompt fixes all 5 with 4 waves of granular sub-agent tasks.
>
> **Existing state:** Stronghold gateway on 45.63.97.103, Rust 1.97.1,
> k3s v1.36.2, 9 images, WebAuthn E2E working, next_work endpoint exists,
> submit_result auto-creates next task, picast project active with 2 agents
> connected.

---

## WAVE CA: Machine Isolation + Task Locking

**Goal:** Each agent gets its own pod. The next_work endpoint locks a task
to the calling machine so two agents never work on the same task.

### CA1 (sub-agent): Task locking in next_work
- **Files:** `gateway/src/routes/next_work.rs`
- **Task:** When `next_work` returns a task, atomically update it from
  `queued` to `running` and set `machine_id` to the calling machine. Use
  `UPDATE ... WHERE status='queued' AND id=?1` to prevent races. If the
  UPDATE affects 0 rows (another agent grabbed it), try the next queued
  task. Return 404 if no unlocked task is available.
- **DoD:**
  - Two concurrent `GET /agent/:machine_id/next-work` calls with different
    machine_ids get DIFFERENT tasks (or one gets 404)
  - The returned task has `status='running'` and `machine_id` set in the DB
  - Unit test: `test_task_locking_prevents_double_assignment`
- **Commit:** `fix(orchestrator): task locking in next_work prevents double assignment (CA1)`

### CA2 (sub-agent): Per-agent pod provisioning
- **Files:** `gateway/src/routes/next_work.rs`, `gateway/src/machines/scheduler.rs`
- **Task:** When `next_work` is called by a machine_id that doesn't exist
  or is expired, automatically schedule a NEW pod for the agent (same
  image, same tenant). Return the new machine_id + connect_token in the
  response. This ensures every agent gets its own pod.
- **DoD:**
  - Agent A calls next_work with machine_X → gets task + machine_X
  - Agent B calls next_work with machine_Y (different) → gets a different task + machine_Y
  - If machine_Y doesn't exist, a new pod is scheduled + returned
  - Two agents NEVER share a machine_id
- **Commit:** `fix(orchestrator): per-agent pod provisioning in next_work (CA2)`

### CA3 (sub-agent): Clean up stale machines
- **Files:** `gateway/src/watchdog/monitor.rs`
- **Task:** The watchdog currently monitors EVERY machine in the DB
  (including 20+ stale machines from prior test runs). Fix `get_active_machines`
  to only return machines that have: (1) `status='active'`, (2) a task
  with `status='running'` or `status='queued'` assigned to them, (3)
  `created_at` within the last 24 hours. Machines with no active tasks
  are ignored (not monitored, no ultimata, no ntfy spam).
- **DoD:**
  - Watchdog log shows 0 "low dedication" warnings for stale machines
  - Only machines with active tasks are monitored
  - ntfy mock log shows 0 anomaly pushes for stale machines
  - Unit test: `test_stale_machines_not_monitored`
- **Commit:** `fix(watchdog): only monitor machines with active tasks (CA3)`

### Wave CA DoD Gate
- `cargo test --features no-sev-snp --lib next_work` → all pass
- `cargo test --features no-sev-snp --lib watchdog` → all pass
- Gateway log shows no watchdog spam for stale machines
- `git push origin main`
- Append `Wave CA: PASS` to worklog

---

## WAVE CB: Continuous Work Loop + Result Reporting

**Goal:** Agents report results. The result response includes the next task.
The agent prompt explicitly loops: work → report → get next → repeat. No EOS.

### CB1 (sub-agent): submit_result returns next task inline
- **Files:** `gateway/src/routes/tasks.rs`
- **Task:** Verify that `submit_result` already auto-creates the next task
  (it was patched earlier). Fix the response to always include `next_work`
  with the full task object (task_id, instruction, phase, machine_id).
  If no next task exists, return `next_work: null` + `status: "idle"`.
  The agent must be able to start the next task from the result response
  alone — no separate next_work poll needed.
- **DoD:**
  - `POST /agent/task/:id/result` returns `{"status":"ok","next_work":{"task_id":"...","instruction":"...","phase":"..."}}`
  - The next_work task is already in `running` state (locked to this machine)
  - If all phases done, returns `{"status":"ok","next_work":null,"message":"all phases complete"}`
  - Unit test: `test_result_returns_next_task`
- **Commit:** `fix(orchestrator): submit_result returns next task inline (CB1)`

### CB2 (sub-agent): Task timeout + auto-result
- **Files:** `gateway/src/routes/tasks.rs`, new background task in `main.rs`
- **Task:** Add a background task that runs every 60s. For each task in
  `running` state, check if the machine has had any `cmd_exec` audit
  entries in the last 5 minutes. If not (agent is frozen/stuck), auto-submit
  a result with `exit_code: -1, summary: "task timed out — agent frozen"`
  and create the next task. This prevents a frozen agent from blocking the
  pipeline forever.
- **DoD:**
  - A task with no exec activity for 5 minutes → auto-completed with timeout
  - The next task is auto-created
  - Audit entry `task_timeout` written
  - Unit test: `test_frozen_task_timeout`
- **Commit:** `feat(orchestrator): task timeout + auto-result for frozen agents (CB2)`

### CB3 (sub-agent): Progress heartbeat endpoint
- **Files:** `gateway/src/routes/tasks.rs`
- **Task:** The existing `POST /agent/task/:id/progress` endpoint exists but
  agents don't call it. Make it return useful info: the current task state,
  how long it's been running, and a "keep going" or "wrap up" signal. If
  the task has been running > 30 minutes, the response includes
  `{"action":"wrap_up","message":"Task running too long, commit what you have and submit result"}`.
- **DoD:**
  - `POST /agent/task/:id/progress` returns `{"status":"stored","action":"keep_going","elapsed_minutes":N}`
  - After 30 min: returns `{"status":"stored","action":"wrap_up","message":"..."}`
  - Unit test: `test_progress_heartbeat_returns_action`
- **Commit:** `feat(orchestrator): progress heartbeat with keep_going/wrap_up signal (CB3)`

### Wave CB DoD Gate
- `cargo test --features no-sev-snp --lib tasks` → all pass
- Manual test: submit a result → next task in response
- `git push origin main`
- Append `Wave CB: PASS` to worklog

---

## WAVE CC: Inter-Agent Communication

**Goal:** Agents communicate via the message bus. When agent A starts a
task, it posts "I'm working on X". When agent B starts, it sees A's message
and picks a different task. Agents post progress visible to each other.

### CC1 (sub-agent): Auto-post task start on message bus
- **Files:** `gateway/src/routes/next_work.rs`
- **Task:** When `next_work` returns a task to an agent, automatically post
  a message on the `project-<tenant_id>` channel:
  `{"role":"agent","type":"task_started","task_id":"...","phase":"...","machine_id":"..."}`
  This lets other agents see what's already being worked on.
- **DoD:**
  - After `GET /agent/:machine_id/next-work`, a message appears on the bus
  - The message has `type: "task_started"` with the task_id + phase
  - Unit test: `test_next_work_posts_task_started`
- **Commit:** `feat(comms): next_work auto-posts task_started on message bus (CC1)`

### CC2 (sub-agent): Auto-post task completion on message bus
- **Files:** `gateway/src/routes/tasks.rs`
- **Task:** When `submit_result` is called, automatically post a message on
  the `project-<tenant_id>` channel:
  `{"role":"agent","type":"task_completed","task_id":"...","phase":"...","exit_code":0,"summary":"..."}`
- **DoD:**
  - After `POST /agent/task/:id/result`, a message appears on the bus
  - The message has `type: "task_completed"` with the summary
  - Unit test: `test_result_posts_task_completed`
- **Commit:** `feat(comms): submit_result auto-posts task_completed on message bus (CC2)`

### CC3 (sub-agent): Agent poll for peer activity
- **Files:** `gateway/src/routes/next_work.rs`
- **Task:** Before assigning a task, `next_work` checks the message bus for
  recent `task_started` messages from other machines. If another agent
  already started the same phase, skip that task and look for the next
  unstarted phase. This prevents two agents from writing the same document.
- **DoD:**
  - Agent A starts ADRs → posts `task_started` for phase `adrs`
  - Agent B calls `next_work` → sees ADRs is taken → gets `fine_draft` instead
  - Unit test: `test_next_work_skips_in_progress_phases`
- **Commit:** `feat(comms): next_work skips phases other agents are working on (CC3)`

### Wave CC DoD Gate
- `cargo test --features no-sev-snp --lib next_work` → all pass
- Manual test: two agents get different tasks
- `git push origin main`
- Append `Wave CC: PASS` to worklog

---

## WAVE CD: Agent Prompt + Commit/Push Pipeline

**Goal:** The agent prompt is a continuous loop that never emits EOS. Agents
commit + push after every task. The orchestrator verifies commits landed.

### CD1 (sub-agent): Fix git commit + push in next_work response
- **Files:** `gateway/src/routes/next_work.rs`
- **Task:** The `next_work` response should include everything the agent
  needs to commit + push after completing the task: the repo path, the
  branch name (auto-generated per phase), and the commit message template.
  Add `repo_path`, `branch`, `commit_message` to the `NextWorkResponse`.
- **DoD:**
  - `next_work` response includes `repo_path: "/home/dev/work/picast"`,
    `branch: "docs/blueprint-adrs"`, `commit_message: "docs: add ADRs (blueprint pipeline)"`
  - The branch is auto-created if it doesn't exist (call git_branch internally)
  - Unit test: `test_next_work_includes_git_info`
- **Commit:** `fix(orchestrator): next_work includes repo path + branch + commit message (CD1)`

### CD2 (sub-agent): Verify commit landed before creating next task
- **Files:** `gateway/src/routes/tasks.rs`
- **Task:** In `submit_result`, before auto-creating the next task, verify
  that the agent actually committed. Check the audit log for a `git_commit`
  event for this machine_id since the task was assigned. If no commit found,
  return `{"status":"error","message":"no commit found for this task — commit before submitting result","next_work":null}`
  and DON'T create the next task. This forces agents to commit before
  moving on.
- **DoD:**
  - `submit_result` without a prior `git_commit` → error, no next task
  - `submit_result` with a prior `git_commit` → ok, next task created
  - Unit test: `test_result_requires_commit`
- **Commit:** `fix(orchestrator): submit_result verifies commit before next task (CD2)`

### CD3 (orchestrator-only): Write the definitive agent loop prompt
- **Files:** `agent/CONTINUOUS_LOOP_PROMPT.md` (new)
- **Task:** Write the final agent prompt that:
  1. Gets a task from `GET /agent/:machine_id/next-work`
  2. Reads relevant files via `POST /agent/:machine_id/exec`
  3. Writes the document via `POST /agent/:machine_id/exec` (base64)
  4. Commits via `POST /agent/:machine_id/git/commit`
  5. Pushes via `POST /agent/:machine_id/git/push`
  6. Reports result via `POST /agent/task/:id/result`
  7. Reads `next_work` from the result response
  8. GO TO 2 with the new task
  9. NEVER stops. NEVER says "I'm done". NEVER emits EOS.
  10. If a task fails, reports the error and gets the next task.
  11. Posts progress every 2 minutes via `POST /agent/task/:id/progress`.
  12. Checks the message bus for peer activity before starting work.
- **DoD:**
  - The prompt is self-contained (an agent can follow it without any other context)
  - The prompt includes the actual curl commands with the real credentials
  - The prompt explicitly says "DO NOT STOP" and "DO NOT EMIT EOS"
  - The prompt includes error recovery (if a command fails, report + continue)
- **Commit:** `docs: definitive continuous-loop agent prompt (CD3)`

### Wave CD DoD Gate
- `cargo test --features no-sev-snp --lib tasks` → all pass
- Manual test: agent follows the loop prompt → commits + pushes + gets next task
- `git push origin main`
- Append `Wave CD: PASS` to worklog

---

## WAVE CE: Live Agent Test + Cleanup

**Goal:** Run 2 agents simultaneously with the fixed infrastructure. Verify
they get different pods, different tasks, communicate via the bus, commit +
push, and stay alive for 10+ minutes.

### CE1 (orchestrator-only): Clean up stale machines + sessions
- **Task:** Delete all stale machines (status != active), clear old pending
  sessions, reset the picast tenant's tasks. Start fresh.
- **DoD:**
  - 0 stale machines in DB
  - 0 old pending sessions
  - Only active machines are the ones from this test
- **Commit:** (no code commit — DB cleanup only)

### CE2 (orchestrator-only): Provision 2 pods + run 2 agents
- **Task:** Pre-provision 2 pods (one per agent). Give each agent the
  continuous loop prompt (from CD3) with its own machine_id + connect_token.
  Both agents should start working on different blueprint phases
  simultaneously.
- **DoD:**
  - 2 pods running (different machine_ids)
  - Agent A works on phase X, Agent B works on phase Y (different)
  - Both agents post `task_started` messages on the bus
  - Both agents commit + push to GitHub (different branches)
  - Both agents report results + get next tasks
  - Both agents stay alive for 10+ minutes (no EOS)
  - Watchdog does NOT spam stale machine warnings
  - Audit trail shows continuous activity from both agents
- **Commit:** `test: live 2-agent continuous loop test (CE2)`

### CE3 (orchestrator-only): Verify GitHub + audit trail
- **Task:** Verify that all blueprint documents landed on GitHub. Verify the
  audit trail is complete + dual-signed. Verify the message bus has
  inter-agent communication.
- **DoD:**
  - `docs/blueprint/01-problem-catalog.md` exists on GitHub
  - `docs/blueprint/02-rough-draft.md` exists on GitHub
  - `docs/blueprint/03-adrs/` exists on GitHub with ≥1 ADR file
  - Audit trail has `task_started`, `cmd_exec`, `git_commit`, `git_push`,
    `task_completed` for both agents
  - All audit entries dual-signed (Ed25519 + ML-DSA-65)
  - Message bus has `task_started` + `task_completed` from both agents
- **Commit:** `test: verify GitHub + audit + message bus for 2-agent test (CE3)`

### Wave CE DoD Gate
- 2 agents stayed alive for 10+ minutes
- Different tasks, different pods, inter-agent communication
- Documents landed on GitHub
- Audit trail complete + dual-signed
- No watchdog spam
- `git push origin main`
- Append `Wave CE: PASS` to worklog

---

## WAVE CF: Hardening + Commit

**Goal:** Fix remaining edge cases, commit all changes, push, tag.

### CF1 (sub-agent): next_work handles "all phases done" gracefully
- **Files:** `gateway/src/routes/next_work.rs`
- **Task:** When all blueprint phases are complete, return
  `{"status":"idle","instruction":"All blueprint phases complete. Review existing documents. If you find issues, fix them and commit."}`
  The agent should stay alive and do review work, not stop.
- **DoD:**
  - All 7 phases complete → `status: "idle"` with review instruction
  - Agent stays alive (doesn't get 404 or error)
  - Unit test: `test_all_phases_done_returns_idle`
- **Commit:** `fix(orchestrator): graceful idle when all phases done (CF1)`

### CF2 (sub-agent): Fix anomaly scanner false positives
- **Files:** `gateway/src/anomaly/scanner.rs`
- **Task:** The anomaly scanner currently fires on normal agent output
  (e.g. an AWS key pattern in a document about AWS). Add a whitelist for
  blueprint document paths — if the exec cwd is `docs/blueprint/`, skip
  anomaly scanning. Only scan exec output for source code files.
- **DoD:**
  - Writing a doc that mentions "AKIA..." → no anomaly
  - Running `cat src/main.rs` that contains "AKIA..." → anomaly detected
  - Unit test: `test_anomaly_scanner_skips_blueprint_docs`
- **Commit:** `fix(anomaly): skip blueprint docs in anomaly scanner (CF2)`

### CF3 (orchestrator-only): Final commit + push + tag
- **Task:** Commit all remaining changes. Run the existing holistic test
  (49/49) + deep test (35/35) to verify no regressions. Tag as
  `v1.3.1-continuous-agents`.
- **DoD:**
  - `cargo test --features no-sev-snp` → all pass
  - `bash scripts/holistic_test.sh` → 49/49
  - `bash scripts/deep_test.sh` → 35/35
  - `git tag v1.3.1-continuous-agents`
  - `git push origin main && git push origin v1.3.1-continuous-agents`
  - Append `Wave CF: PASS — v1.3.1-continuous-agents tagged` to worklog
- **Commit:** (the tag is the commit)

### Wave CF DoD Gate
- All tests pass
- `v1.3.1-continuous-agents` tagged + pushed
- Append `Wave CF: PASS` to worklog

---

## ORCHESTRATOR RETURN CONTRACT

The orchestrator may return only when ALL of the following are true:

1. ✅ Wave CA: Machine isolation (different pods), task locking (no double assignment), watchdog only monitors active machines
2. ✅ Wave CB: submit_result returns next task inline, frozen task timeout, progress heartbeat
3. ✅ Wave CC: Inter-agent communication (task_started/task_completed on bus, next_work skips in-progress phases)
4. ✅ Wave CD: next_work includes git info, submit_result verifies commit, definitive continuous-loop agent prompt
5. ✅ Wave CE: 2 agents live for 10+ min, different pods/tasks, docs on GitHub, audit complete
6. ✅ Wave CF: Edge cases fixed, tests pass, v1.3.1-continuous-agents tagged
7. ✅ All commits pushed to `origin/main`
8. ✅ Worklog updated with per-wave PASS entries

**If any DoD fails:** Diagnose → patch → re-test → re-commit → re-push. No returning on red.

---

## SUB-AGENT RULES

- Each task ≤ 500 lines
- Sub-agents read ONLY: task description + files to modify + this prompt section
- Sub-agents append to worklog before returning
- Use `python3 /home/z/my-project/scripts/ssh_exec.py '<command>'` for dev box access
- Dependencies: latest stable only (Rust 1.97.1, k3s v1.36.2, all already provisioned)

---

## THE 5 PROBLEMS THIS FIXES

| # | Problem | Wave | Fix |
|---|---------|------|-----|
| 1 | Both agents on same machine | CA1+CA2 | Task locking + per-agent pod provisioning |
| 2 | ADRs task never reported | CB1+CB2 | submit_result returns next task + frozen task timeout |
| 3 | Nothing landed on GitHub | CD1+CD2 | next_work includes git info + submit_result verifies commit |
| 4 | Watchdog spams stale machines | CA3 | Only monitor machines with active tasks |
| 5 | No inter-agent communication | CC1-CC3 | Auto-post task_started/completed + skip in-progress phases |
