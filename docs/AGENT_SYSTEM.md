# Stronghold Multi-Agent System

This document is the canonical reference for Stronghold's multi-agent coding
platform — the architecture, the agent roles, the team strategies, the
watchdog system, the communication protocols, the constitutional
principles, the reflexion loop, the re-planning protocol, the workflow
template catalog, and the HTTP API surface that binds them together.

It synthesises material from:

- `agent/protocols/agent-architecture.md` — research-grounded role catalog
  and team strategies
- `agent/protocols/multi-agent-coding.md` — execution flow and message bus
- `agent/prompts/*.md` — per-role system prompts (9 roles)
- `agent/templates/*.json` — 12 ready-to-run workflow DAGs
- `gateway/src/watchdog/*` — dedication scoring, workaround detection,
  ultimatum escalation
- `gateway/src/workflow/*` — DAG executor
- `gateway/src/routes/*` — HTTP endpoints

> **Scope.** This document covers the *agent* layer. For the control-plane
> layer (sessions, attestation, crypto, multi-tenancy), see
> [PROTOCOL.md](PROTOCOL.md), [CRYPTO.md](CRYPTO.md), and [SEV_SNP.md](SEV_SNP.md).

---

## 1. Architecture Overview

Stronghold runs **multiple AI agents concurrently inside hardened,
phone-approved, SEV-SNP attested VMs** on a k3s worker plane. Each agent is
a separate pod with its own OCI image, its own git checkout, and its own
role-scoped tool permissions. Agents communicate via a persisted message
bus (`agent_messages` table) and coordinate via **workflow DAGs** stored in
the `workflows` / `workflow_runs` tables.

```
                    ┌─────────────────────────────────────────┐
                    │              Human (Phone)              │
                    │  approve · reprompt · revoke · extend   │
                    └────────┬───────────────────────┬────────┘
                             │ /phone/decide         │ /phone/revoke
                             ▼                       ▼
   ┌──────────────────────────────────────────────────────────────────┐
   │                    Stronghold Gateway                            │
   │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
   │  │  Tenant  │  │  Quotas  │  │  Audit   │  │  Credential      │  │
   │  │ Registry │  │  /Auth   │  │  Log     │  │  Vault (AES-GCM) │  │
   │  └──────────┘  └──────────┘  └────┬─────┘  └──────────────────┘  │
   │                                   │                              │
   │  ┌────────────────────────────────┼──────────────────────────┐  │
   │  │         Workflow Engine (DAG executor)                     │  │
   │  │   plan → implement → test → review → merge                │  │
   │  └────────────────┬───────────────┬───────────────────────────┘  │
   │                   │               │                              │
   │  ┌────────────────────────┐  ┌────▼─────────────────────────┐    │
   │  │   Watchdog Loop        │  │  Agent Message Bus           │    │
   │  │  (60s cycle, per       │  │  (SQLite-backed channels,    │    │
   │  │   active machine)      │  │   SSE + poll)                │    │
   │  │  • dedication score    │  │                              │    │
   │  │  • workaround detect   │  │  Channels:                   │    │
   │  │  • ultimata L1/L2/L3   │  │   • workflow-run-<id>        │    │
   │  └────────────────────────┘  │   • oracle-<machine_id>      │    │
   │                              │   • facilitator-<...>        │    │
   │                              │   • escalation               │    │
   │                              └──────────────────────────────┘    │
   └──────────────────────────────────────────────────────────────────┘
                             │ k3s pod scheduling
                             ▼
   ┌──────────────────────────────────────────────────────────────────┐
   │                  k3s Worker Plane (per-machine pods)             │
   │  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐      │
   │  │Planner │  │ Coder  │  │Tester  │  │Review- │  │Integra-│      │
   │  │        │  │        │  │        │  │  er    │  │  tor   │      │
   │  └────────┘  └────────┘  └────────┘  └────────┘  └────────┘      │
   │  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐                  │
   │  │Watchdog│  │ Oracle │  │Architec│  │Facilit-│                  │
   │  │        │  │        │  │  t     │  │  ator  │                  │
   │  └────────┘  └────────┘  └────────┘  └────────┘                  │
   │                                                                  │
   │  Each pod: OCI image · git checkout · role-scoped tools · TTL    │
   │  PTY proxied via WebSocket; exec via JSON endpoint               │
   └──────────────────────────────────────────────────────────────────┘
```

### Data model at a glance

| Table | Purpose |
|---|---|
| `tenants` | Multi-tenant isolation root. Every other table has `tenant_id`. |
| `machines` | Active/recent agent pods. Status: `active`/`released`/`revoked`/`expired`/`lost`. |
| `tasks` | Structured work units. Lifecycle: `queued` → `scheduled` → `running` → `completed`/`failed`/`cancelled`. |
| `workflows` | Named DAG definitions (`name`, `dag` JSON, `status = draft`). |
| `workflow_runs` | Execution instances of a workflow (`status`, `current_steps`, `completed_steps`). |
| `task_outputs` | Key/value store per task — used for progress reports (`progress_<ts>`), reflexions (`reflexion`), and artifacts. |
| `agent_credentials` | Encrypted (AES-256-GCM) secrets the Coder/Integrator can fetch at runtime. |
| `agent_messages` | The inter-agent message bus — `from_machine`, `to_machine` (NULL = broadcast), `channel`, JSON `body`. |
| `audit_entries` | Hash-chained, dual-signed (Ed25519 + ML-DSA-65) audit log. |
| `agent_roles` | Per-tenant role definitions: `system_prompt`, `allowed_tools`, `denied_tools`. |
| `disagreements` | Coder-vs-Reviewer disputes the Facilitator mediates. |
| `watchdog_reports` | Per-cycle dedication scores + workaround warnings. |
| `ultimata` | Level 2/3 ultimata rows (Level 1 is fire-and-forget). |
| `workflow_templates` | Per-tenant copies of the 12 standard templates. |

---

## 2. Agent Roles (9 roles)

Stronghold ships **9 default roles**, seeded per tenant via
`POST /admin/roles/seed`. Each role has a `system_prompt` (the agent's
persona), an `allowed_tools` whitelist (empty = allow all), and a
`denied_tools` blacklist (always wins). The full prompts live in
`agent/prompts/<role>.md`; the gateway stores compact one-paragraph
summaries in `agent_roles.system_prompt`.

### 2.1 Planner

**Mission.** Decompose the task, explore the codebase, produce an
implementation plan and a workflow DAG.

**Tools.** `git_clone`, `exec` (read-only), `workflow_create`, `result`.
Denied: `git_branch`, `git_commit`, `git_push`, `git_pr` — the Planner
never writes code.

**Reference.** ReAct (Yao et al., 2023) — interleaved reasoning and
acting.

### 2.2 Coder

**Mission.** Implement the plan: clone, branch, write code, run tests
locally, commit, push, open a PR. Respond to reviewer feedback by fixing
issues and re-pushing.

**Tools.** Full git suite: `git_clone`, `git_branch`, `exec`,
`git_commit`, `git_push`, `git_pr`, `result`. Plus credential access via
`GET /agent/:machine_id/credentials/:name` (e.g. `github-pat`).

**Reference.** CodeAct (Wang et al., 2024) — executable code actions as
the agent's primary lever.

### 2.3 Reviewer

**Mission.** Review code diffs. Check correctness, security, tests, error
handling, performance, style. Approve or request changes.

**Tools.** `git_clone`, `exec` (read-only), `result`. Denied: all write
git operations — the Reviewer never creates branches, pushes, or merges.

**Reference.** Self-Refine (Madaan et al., 2023) — iterative refinement
via self-feedback.

### 2.4 Tester

**Mission.** Check out the PR branch, run the test suite / lint / format
checks, parse results, post structured `test_results` on the bus.

**Tools.** `git_clone`, `exec` (test commands only), `result`. Denied:
all write git operations.

**Reference.** AutoTest (Schäfer et al., 2024) — automated test
generation and execution.

### 2.5 Integrator

**Mission.** Merge approved PRs, run CI on `main`, keep `main` green.
Verify review approval + passing tests, check for conflicts, merge with
`--squash --delete-branch`, run CI, post `integration_complete` or
`integration_failed`. Never force-merge conflicts.

**Tools.** `git_clone`, `exec`, `result`. Denied: `git_branch`,
`git_commit` — the Integrator merges existing PRs but doesn't author
code.

### 2.6 Watchdog

**Mission.** Monitor other agents for dedication, progress, workarounds,
and scope reduction. Every 60 seconds: compute a dedication score, scan
for workaround patterns, issue escalating ultimata (Level 1 warning,
Level 2 directive, Level 3 escalation) when agents drift off-task.

**Tools.** `exec`, `result`. Denied: all git operations and
`workflow_create` — the Watchdog never writes code or modifies the plan.

**Reference.** MetaGPT watchdogs (Hong et al., 2023). See §4 below for
the full watchdog spec.

### 2.7 Oracle

**Mission.** Answer codebase questions from other agents. The team's
collective memory and search engine. Read-only git access; can run
read-only commands (`grep`, `find`, `cat`, `rg`, `fd`). Listens on
channel `oracle-<machine_id>` for `question` messages, replies with
`answer` messages including file paths + line numbers + code snippets.

**Tools.** `git_clone`, `exec` (read-only), `result`. Denied: all write
git operations.

**Reference.** RAG-based retrieval (Lewis et al., 2020).

### 2.8 Architect

**Mission.** Make system design decisions before implementation begins.
Bridge the gap between the Planner's high-level plan and the Coder's
detailed implementation. Evaluate design options, define interfaces
(function signatures, type definitions, module structure, error types),
identify risks (breaking changes, migrations, perf regressions, security
implications), document the design (ASCII diagram + data flow +
interfaces + test strategy).

**Tools.** `git_clone`, `exec` (read-only), `result`. Denied: all write
git operations.

**Reference.** ChatDev architect role (Qian et al., 2024).

### 2.9 Facilitator

**Mission.** Mediate disagreements between agents (typically Coder vs
Reviewer). Analyze both sides, reference codebase conventions and best
practices, make a **binding** decision with reasoning and precedent.
Decisions are final unless overturned by a human.

**Tools.** `git_clone`, `exec` (read-only), `result`. Denied: all write
git operations.

**Reference.** Multi-agent debate (Du et al., 2023).

### Role catalog summary

| # | Role | Writes code? | Merges? | Monitors? | Reference |
|---|---|---|---|---|---|
| 1 | Planner | ✗ | ✗ | ✗ | ReAct |
| 2 | Coder | ✓ | ✗ | ✗ | CodeAct |
| 3 | Reviewer | ✗ | ✗ | ✗ | Self-Refine |
| 4 | Tester | ✗ | ✗ | ✗ | AutoTest |
| 5 | Integrator | ✗ | ✓ | ✗ | — |
| 6 | Watchdog | ✗ | ✗ | ✓ | MetaGPT |
| 7 | Oracle | ✗ | ✗ | ✗ | RAG |
| 8 | Architect | ✗ | ✗ | ✗ | ChatDev |
| 9 | Facilitator | ✗ | ✗ | ✗ | Multi-agent debate |

---

## 3. Team Strategies (5 strategies)

The Planner selects a strategy based on task type. Each strategy maps to
a workflow template (see §9).

### Strategy A — Hierarchical Delegation (default)

```
Human → Planner → Coder(s) → Reviewer → Integrator
                ↑ Watchdog monitors all ↓
```

The Planner is the team lead. It decomposes the task, assigns sub-tasks
to Coders, coordinates with the Reviewer, and hands off to the
Integrator. The Watchdog runs in parallel, monitoring all agents.

**When to use.** Complex multi-file tasks, refactors, new features.

**Template.** `standard-cicd`.

### Strategy B — Debate-Based Consensus

```
Coder-A → solution-1 ↘
Coder-B → solution-2 → Reviewer → Facilitator → winning solution
Coder-C → solution-3 ↗
```

Multiple Coders independently implement the same task. The Reviewer
compares solutions. The Facilitator mediates if Coders disagree on
approach. The best solution (by test pass rate, code quality, and
Reviewer judgment) wins.

**When to use.** Hard bugs with multiple possible approaches, algorithm
design, security-sensitive changes.

**Templates.** `debate-bugfix`, `tournament`.

### Strategy C — Tournament (competitive)

```
Coder-A → PR-A → Tester → score-A ↘
Coder-B → PR-B → Tester → score-B → highest score wins → Integrator
Coder-C → PR-C → Tester → score-C ↗
```

Multiple Coders compete. Each solution is scored by:
- Test pass rate (40%)
- Code quality score from Reviewer (30%)
- Performance benchmark (15%)
- Code size / simplicity (15%)

The highest-scoring solution is merged. Others are discarded.

**When to use.** Performance optimization, algorithm challenges,
proof-of-concept implementations.

**Template.** `tournament`.

### Strategy D — Pipeline (sequential refinement)

```
Coder → Draft → Reviewer → Feedback → Coder → Revise → Reviewer → Approve → Tester → CI → Integrator
```

Single Coder iterates with Reviewer until approved. Strict quality gate
— no PR is merged until the Reviewer explicitly approves. Watchdog
monitors for stagnation (same issue raised 3+ times = escalate).

**When to use.** Critical bug fixes, security patches, production
hotfixes.

**Templates.** `hotfix`, `bug-fix-fast`, `security-audit`.

### Strategy E — Mixture of Experts

```
Task → Router → [Architect | Coder | Tester | Reviewer | DevOps]
                   ↑ each expert handles their specialty ↓
                 Oracle (answers codebase questions for all)
                 Watchdog (monitors all)
```

A Router agent reads the task and dispatches to the appropriate
specialist. The Oracle answers codebase questions for any agent. The
Watchdog monitors all.

**When to use.** Open-ended tasks where the work type isn't known
upfront.

**Templates.** `onboarding`, `doc-sprint`, `dep-upgrade`,
`perf-regression`, `continuous-improvement`, `multi-component-refactor`.

---

## 4. Watchdog System

The Watchdog is the most critical role. It doesn't write code — it
watches other agents and enforces focus. The implementation lives in
`gateway/src/watchdog/`:

- `dedication.rs` — dedication scoring engine
- `detector.rs` — workaround detector (diff scanning + spin detection)
- `ultimatum.rs` — ultimatum issuance + acknowledgment checking
- `monitor.rs` — the 60-second monitoring loop (`spawn_watchdog`)

### 4.1 Dedication Score

Computed every 60 seconds for each active machine:

```
dedication = (relevant_commands / total_commands) × progress_rate × task_alignment
```

| Signal | Source | Range |
|---|---|---|
| `relevant_commands` | Count of recent commands matching task keywords | `[0, total]` |
| `total_commands` | Count of all recent commands | `≥ 0` |
| `progress_rate` | `min(1.0, (files_changed + tests_run + commits) / 5)` | `[0.0, 1.0]` |
| `task_alignment` | `1.0` if any recent command touched a task keyword, `0.5` otherwise, `0.0` if no activity | `{0.0, 0.5, 1.0}` |

**Edge cases.**

- Empty audit log → `score = 0.0`, `task_alignment = 0.0`.
- All-relevant → `task_alignment = 1.0`, `score = progress_rate`.
- None-relevant → `task_alignment = 0.5`, `score = 0.0` (the relevance
  factor zeroes the product).

**Progress heuristics** (`ProgressIndicators::from_audit_entries`):

- `files_changed` — command contains `git diff` or `git add`, **or** the
  event name indicates a file write (`file_write`, `fs_write`,
  `write_file`).
- `tests_run` — command contains `cargo test`, `npm test`, or `pytest`.
- `commits` — command contains `git commit`.
- `last_activity_secs` — approximate; ~1 audit entry per 5 seconds of
  wall-clock activity.

### 4.2 Workaround Detection

`detect_workarounds(recent_output, git_diff)` scans **only newly added
diff lines** (those beginning with `+`, excluding the `+++ b/file`
header) so pre-existing surrounding code never produces warnings.

| # | Pattern | Severity | `pattern` id |
|---|---|---|---|
| 1 | `.unwrap()` / `.expect(` | high | `unwrap_call` / `expect_call` |
| 2 | `#[allow(dead_code)]` / `#[allow(clippy::*)]` | high | `allow_dead_code` / `allow_clippy` |
| 3 | `#[ignore]` on tests | high | `ignored_test` |
| 4 | `todo!()` / `unimplemented!()` | critical | `todo_macro` / `unimplemented_macro` |
| 5 | `// TODO` / `// FIXME` | medium | `todo_comment` / `fixme_comment` |
| 6 | `println!` / `dbg!` | medium | `println_macro` / `dbg_macro` |
| 7 | same shell command 3+ times (spin) | high | `spin` |
| 8 | empty function body | high | `empty_function_body` |

**Spin detection** (#7) operates on `recent_output`: it extracts the
command text following each shell prompt (`$`/`#`/`%` + whitespace) and
flags any command repeated three or more times.

Each warning is a `WorkaroundWarning` struct:

```json
{
  "pattern": "unwrap_call",
  "severity": "high",
  "file": "src/main.rs",
  "line": 42,
  "message": "new code calls .unwrap() — will panic on None/Err"
}
```

### 4.3 Ultimata

When `dedication < 0.3` for consecutive checks, the Watchdog issues
escalating ultimata:

| Level | Variant | Trigger | Persisted? | Bus post? | Phone push? |
|---|---|---|---|---|---|
| 1 | `Warning` | 3 consecutive low-dedication checks | no | no | no |
| 2 | `Directive` | 5 consecutive (2 after Level 1) | yes (`acknowledged = 0`) | no | no |
| 3 | `Escalation` | 7 consecutive (2 after Level 2, or no ACK) | yes | yes (`escalation` channel) | yes |

**Level 1 (Warning).** A soft nudge injected into the agent's PTY as a
structured control message (OSC escape sequence, same envelope as the
mid-session reprompt in `instruct.rs`). Fire-and-forget — nothing is
persisted.

**Level 2 (Directive).** The same PTY injection, plus a row in the
`ultimata` table with `acknowledged = 0`. The agent is expected to
acknowledge by running `echo ACK_TASK_FOCUS`; the acknowledgment is
detected by `check_ultimatum_acknowledgment` scanning the audit log for
`cmd_exec` events whose payload contains `ACK_TASK_FOCUS` and whose
timestamp is after the ultimatum's `created_at`.

**Level 3 (Escalation).** Level 2 plus an escalation message on the
`agent_messages` bus (channel `escalation`, addressed to the Planner)
and a best-effort phone push notification. Used when the agent has not
acknowledged a Level 2 directive within the deadline.

**Acknowledgment protocol.**

```rust
pub fn check_ultimatum_acknowledgment(
    db: &Pool<SqliteConnectionManager>,
    ultimatum_id: i64,
) -> bool
```

Returns `true` if at least one `cmd_exec` audit entry exists for the
ultimatum's `target_machine` whose payload `LIKE '%ACK_TASK_FOCUS%'` and
whose `ts > ultimatum.created_at`. Fail-closed: any DB error returns
`false` (a transient blip doesn't silently mark an ultimatum as
acknowledged).

**Recovery.** If `dedication ≥ 0.3` again, the Watchdog resets the
consecutive-low counter and the ultimatum level for that machine.

### 4.4 Watchdog Report Format

Posted every 60 seconds to channel `workflow-run-<run_id>`:

```json
{
  "type": "watchdog_report",
  "watched_machine": "mach_01HXYZ",
  "watched_task_id": "task_01HXYZ",
  "dedication_score": 0.72,
  "progress_indicators": {
    "files_changed": 3,
    "tests_run": 5,
    "commits": 1,
    "last_activity_seconds_ago": 15
  },
  "workaround_warnings": [],
  "ultimatum_level": 0,
  "assessment": "Agent is on-task and making steady progress."
}
```

The assessment string is derived from the score:
- `> 0.7` → "Agent is on-task and making progress."
- `> 0.3` → "Agent is partially focused."
- `≤ 0.3` → "Agent appears off-task or stuck."

---

## 5. Communication Protocols

Agents communicate via the `agent_messages` table (migration 003). A
message has `from_machine`, optional `to_machine` (NULL = broadcast), a
`channel` string, and an arbitrary JSON `body`. Recipients poll with
`GET /agent/:machine_id/messages` or subscribe to a live SSE stream at
`GET /agent/:machine_id/messages/stream`.

### 5.1 Question-Answer (Oracle) Protocol

```json
// Coder → Oracle (channel: oracle-<machine_id>)
{
  "question_id": "oq_01HX...",
  "type": "question",
  "question": "Where is the token validation logic?",
  "context": { "task_id": "task_01HXYZ" },
  "tenant_id": "tenant_...",
  "machine_id": "mach_..."
}

// Oracle → Coder (same channel, correlated by question_id)
{
  "type": "answer",
  "from": "oracle",
  "to": "<requesting_machine_id>",
  "question": "Where is the token validation logic?",
  "answer": "Token validation is in src/auth.rs:validate_token() at line 42. It calls jwt::decode() and checks the claims. Expiry is NOT currently checked.",
  "references": [
    "src/auth.rs:42 — validate_token() function",
    "src/auth.rs:87 — jwt::decode() call"
  ]
}
```

HTTP surface: `POST /agent/:machine_id/oracle` (ask) →
`GET /agent/:machine_id/oracle/:question_id` (poll for answer).

### 5.2 Progress Report Protocol

```json
// Coder → channel (workflow-run-<run_id> or workflow-task-<task_id>)
{
  "type": "progress",
  "from": "coder",
  "task_id": "task_01HXYZ",
  "key": "progress_1700000000000",
  "tenant_id": "tenant_...",
  "summary": {
    "tests_run": 4,
    "tests_passing": 4,
    "commits": 1,
    "blockers": [],
    "status": "on_track",
    "files_changed_count": 2
  }
}
```

The full report is persisted in `task_outputs` under key
`progress_<unix_ms>` (millisecond-precision, so each report gets a
unique key and the full history is preserved). HTTP surface:
`POST /agent/task/:id/progress`.

### 5.3 Help Request Protocol

```json
// Coder → channel: help needed
{
  "type": "help_request",
  "from": "coder",
  "task_id": "task_01HXYZ",
  "question": "The test for token expiry is flaky — it depends on system clock. Should I use mock time?",
  "context": { "test_file": "tests/auth_test.rs", "line": 42 }
}

// Planner → Coder: guidance
{
  "type": "help_response",
  "from": "planner",
  "to": "coder",
  "task_id": "task_01HXYZ",
  "answer": "Yes, use mock_time crate. Add it to Cargo.toml dev-dependencies. Mock chrono::Utc::now() in the test."
}
```

### 5.4 Disagreement Protocol (Facilitator)

When the Coder and Reviewer can't agree, either party submits a
disagreement to the Facilitator:

```json
// Coder → Facilitator
{
  "type": "disagreement",
  "from": "coder",
  "to": "facilitator",
  "task_id": "task_01HXYZ",
  "issue": "Reviewer says to use Result<> but I think Option<> is cleaner here. The function never errors — it either finds a token or doesn't.",
  "context": {
    "file": "src/auth.rs",
    "line": 42,
    "reviewer_comment": "Use Result<Claims, AuthError> instead of Option<Claims>"
  }
}

// Facilitator → both (binding decision)
{
  "type": "facilitation_decision",
  "from": "facilitator",
  "to": "channel",
  "task_id": "task_01HXYZ",
  "decision": "Use Result<Claims, AuthError>. Three reasons: 1) The codebase uses Result<> for all fallible operations (see src/db/mod.rs:42, src/crypto/hybrid_sig.rs:87). 2) Even if the only current error is 'not found', future error variants can be added without breaking the API. 3) Using Option<> for something that is semantically an error is an anti-pattern.",
  "reasoning": "Codebase consistency (40%): Result<> is the established pattern. Correctness (25%): Result is semantically correct. Maintainability (20%): Result allows adding error variants without API breakage. Performance (10%): No measurable difference. Preference (5%): N/A.",
  "precedent": "Use Result<T, E> for all fallible operations, even if there is currently only one error variant.",
  "binding": true
}
```

The Facilitator's decision framework:

| Factor | Weight |
|---|---|
| Codebase consistency | 40% |
| Correctness | 25% |
| Maintainability | 20% |
| Performance | 10% |
| Personal preference | 5% |

HTTP surface: `POST /agent/:machine_id/disagreement` (submit) →
`GET /agent/:machine_id/disagreement/:id` (poll for decision). Decisions
are persisted in `disagreements` with `decision`, `reasoning`, and
`precedent` columns, building a precedent database over time.

---

## 6. Constitutional Principles (10 rules)

All agents operate under these 10 constitutional principles, injected as
a system-prompt preamble regardless of role. Sourced from
`agent/protocols/agent-architecture.md` §5 (reference: Constitutional AI,
Bai et al., 2022). Returned verbatim by `GET /admin/constitution`.

| # | Principle | Description |
|---|---|---|
| 1 | **Correctness over speed** | A slow correct solution is better than a fast broken one. |
| 2 | **Honesty about uncertainty** | If you're not sure, say so. Don't fabricate APIs or functions. |
| 3 | **No workarounds** | Don't suppress warnings, skip tests, or add `#[allow(...)]` to make code compile. Fix the root cause. |
| 4 | **Minimal changes** | Change only what's needed. Don't refactor unrelated code in the same PR. |
| 5 | **Test what you change** | Every code change must have corresponding tests. |
| 6 | **Fail loud** | If something is wrong, raise an error. Don't silently return defaults. |
| 7 | **Document public APIs** | Every public function must have a doc comment. |
| 8 | **Respect the codebase** | Match existing conventions, style, and patterns. |
| 9 | **No secrets in code** | Use environment variables. Never hardcode tokens, passwords, or keys. |
| 10 | **Escalate when stuck** | After 3 failed attempts, ask for help. Don't spin indefinitely. |

Principle 3 is enforced at runtime by the Watchdog's workaround detector
(§4.2). Principle 10 is enforced by the spin detector (pattern #7) and
the ultimatum escalation ladder (§4.3).

---

## 7. Reflexion Loops

After each task completion (or failure), the agent performs a structured
self-reflection, inspired by the Reflexion paper (Shinn et al., 2023).
The reflexion is submitted via `POST /agent/task/:id/reflexion` and
stored in `task_outputs` under the constant key `"reflexion"` (one per
task; resubmission overwrites).

```json
{
  "what_went_well": "Plan was clear; implementation went smoothly; tests passed first try.",
  "what_went_wrong": "Initial clone was slower than expected due to large history.",
  "what_differently": "Use a shallow clone (--depth 1) for ephemeral CI runs.",
  "what_learned": "Workflow conditions on exit_code are a clean way to gate downstream steps."
}
```

The reflexion is retrievable via `GET /agent/task/:id/reflexion`. The
Planner can query past reflexions via `GET /agent/reflexions?limit=20`
to avoid repeating mistakes on similar tasks — closing the
**reflexion → future planning** feedback loop.

The stored value also includes `tenant_id` and a `ts` RFC-3339 timestamp
for ordering and auditing.

---

## 8. Re-Planning Protocol

When a task fails and retries are exhausted, the Planner re-plans
(reference: Plan-and-Solve Prompting, Wang et al., 2023):

1. **Analyze failure.** Read the failed task's result, audit log, and
   reflexion.
2. **Determine cause.** Was it a bad plan, insufficient context, wrong
   approach, or external dependency?
3. **Adjust plan.** Modify the DAG — add steps, change instructions,
   increase TTL, change agent role.
4. **Restart.** Create a new workflow run with the modified DAG.

```json
// Planner → channel: re-planning
{
  "type": "replan",
  "from": "planner",
  "original_task_id": "task_01HXYZ",
  "reason": "Coder agent failed to implement JWT expiry check after 3 retries. Reflexion indicates the agent didn't understand the jwt crate API. New plan includes a research step.",
  "new_workflow": {
    "steps": [
      {
        "id": "research",
        "task": { "instruction": "Read the jwt crate documentation. Understand how to decode and validate JWT tokens. Specifically find how to check expiry (exp claim).", "image": "stronghold/rust-nightly", "ttl_secs": 600 },
        "depends_on": []
      },
      {
        "id": "implement",
        "task": { "instruction": "Using the research from the previous step, implement JWT expiry checking in src/auth.rs:validate_token().", "image": "stronghold/rust-nightly", "ttl_secs": 3600 },
        "depends_on": ["research"]
      }
    ]
  }
}
```

The re-planning decision is logged to the audit log as `workflow_failed`
(on the original run) followed by a new `workflow_run_started`. Past
reflexions (§7) inform the new plan.

---

## 9. Workflow Templates (12 templates)

Stronghold ships 12 ready-to-run workflow DAGs in
`agent/templates/*.json`. Each template is a JSON object with a `name`
and a `dag` containing a `steps` array. Each step has:

- `id` — unique step identifier within the workflow
- `task` — object with `instruction`, `image`, `ttl_secs`
- `depends_on` — array of step IDs that must complete first
- `role` (optional) — agent role to assign (`planner`, `coder`, etc.)
- `condition` (optional) — gating expression like `prev.result.exit_code == 0`

Templates are validated by `gateway/tests/template_test.rs` — every
template must parse as valid JSON, have non-empty `name`/`steps`, every
step must have `id`/`task.instruction`/`task.image`/`task.ttl_secs`, all
`depends_on` references must point to existing step IDs, no cycles, and
all `condition`/`parallel_with` references must be valid.

### 9.1 `standard-cicd` — Strategy A (Hierarchical Delegation)

The default CI/CD pipeline. Linear: plan → implement → test → review →
merge. Each step gates on the previous step's `exit_code == 0` (or
`approved == true` for merge).

```
plan → implement → test → review → merge
```

5 steps. Roles: planner, coder, tester, reviewer, integrator.

### 9.2 `hotfix` — Strategy D (Pipeline)

Emergency hotfix pipeline. Minimal: fix → review-merge → deploy. Tight
TTLs (900–1800s). Uses `stronghold/fullstack` image for the deploy step.

```
fix → review-merge → deploy
```

3 steps. No conditions (every step runs unconditionally — speed over
gating).

### 9.3 `bug-fix-fast` — Strategy D (Pipeline)

Fast bug fix: fix-and-test → review-and-merge. Single Coder does both
fix and test in one step; Reviewer merges if `exit_code == 0`.

```
fix-and-test → review-and-merge
```

2 steps. Roles: coder, reviewer. Condition on `fix-and-test.result.exit_code == 0`.

### 9.4 `debate-bugfix` — Strategy B (Debate)

Two independent fix approaches, tested in parallel, judged by score.
Winner is merged; loser is closed with explanation.

```
        ┌→ solution-a → test-a ─┐
analyze ┤                        ├→ judge → merge
        └→ solution-b → test-b ─┘
```

7 steps. No roles assigned (any role can fill in). Scoring: correctness
(40%), coverage (30%), quality (20%), minimalism (10%).

### 9.5 `tournament` — Strategy C (Tournament)

Three independent implementations compete. Each is tested + benchmarked.
Judge scores on tests (40%), quality (30%), perf (15%), simplicity
(15%). Winner is merged.

```
        ┌→ implement-a → test-a ─┐
        │                         │
        ├→ implement-b → test-b ─┼→ judge → merge
        │                         │
        └→ implement-c → test-c ─┘
```

8 steps. Most parallel structure of any template.

### 9.6 `multi-component-refactor` — Strategy E (Mixture of Experts)

Refactor core, API, and UI layers in parallel, then integrate + review +
merge. Uses `stronghold/node-20` for the UI step, `stronghold/rust-nightly`
for the rest.

```
        ┌→ refactor-core ─┐
plan ───┼→ refactor-api  ─┼→ integrate → review → merge-to-main
        └→ refactor-ui   ─┘
```

7 steps. Roles: planner, coder (×3), integrator, reviewer, integrator.

### 9.7 `security-audit` — Strategy D (Pipeline)

Run security scans, analyze findings, fix criticals (gated on
`critical_count > 0`), review fixes, re-scan.

```
scan → analyze → fix-critical → review → re-scan
                           ↑
              condition: analyze.result.critical_count > 0
```

5 steps. The `fix-critical` step has a condition that gates it on
critical findings existing — if there are none, the step is skipped and
`review`/`re-scan` proceed (the engine records skipped steps as
completed with `{"result":{"skipped":true}}`).

### 9.8 `perf-regression` — Strategy E (Mixture of Experts)

Benchmark → bisect (gated on regressions > 0) → analyze → fix → verify.

```
benchmark → bisect → analyze → fix → verify
                ↑
       condition: benchmark.result.regressions > 0
```

5 steps. The `bisect` step only runs if the benchmark detects
regressions; otherwise it's skipped.

### 9.9 `dep-upgrade` — Strategy E (Mixture of Experts)

Check outdated → upgrade patch → upgrade minor → test → review → PR.
Sequential to catch breaking changes early.

```
check → patch → minor → test → review → pr
```

6 steps. No conditions — every step runs (the test step would fail and
halt the run if a patch broke something).

### 9.10 `doc-sprint` — Strategy E (Mixture of Experts)

Audit docs → fan out to fix-comments / update-readme / changelog →
verify links. Three parallel doc-fixing branches converge on a verify
step.

```
        ┌→ fix-comments ─┐
audit ──┼→ update-readme ─┼→ verify
        └→ changelog     ─┘
```

5 steps. All `stronghold/rust-nightly` image.

### 9.11 `onboarding` — Strategy E (Mixture of Experts)

Analyze codebase for new contributors. Structure → fan out to entry /
apis / patterns → synthesize into CODEBASE_GUIDE.md.

```
        ┌→ entry      ─┐
structure┼→ apis       ─┼→ synthesize
        └→ patterns   ─┘
```

5 steps. Pure read-only analysis — no roles assigned (any agent can run
these).

### 9.12 `continuous-improvement` — Strategy E (Mixture of Experts)

The reflexion cycle, automated. Analyze last 20 failed tasks → propose
prompt/template improvements → update templates → review improvements.

```
analyze-failures → improve-prompts → update → review
```

4 steps. This template uses the reflexion data (§7) as its input —
closing the loop between reflexion and template evolution.

### Template summary

| # | Template | Strategy | Steps | Conditions | Roles used |
|---|---|---|---|---|---|
| 1 | `standard-cicd` | A | 5 | 3 | planner, coder, tester, reviewer, integrator |
| 2 | `hotfix` | D | 3 | 0 | — |
| 3 | `bug-fix-fast` | D | 2 | 1 | coder, reviewer |
| 4 | `debate-bugfix` | B | 7 | 0 | — |
| 5 | `tournament` | C | 8 | 0 | — |
| 6 | `multi-component-refactor` | E | 7 | 1 | planner, coder, integrator, reviewer |
| 7 | `security-audit` | D | 5 | 1 | — |
| 8 | `perf-regression` | E | 5 | 1 | — |
| 9 | `dep-upgrade` | E | 6 | 0 | — |
| 10 | `doc-sprint` | E | 5 | 0 | — |
| 11 | `onboarding` | E | 5 | 0 | — |
| 12 | `continuous-improvement` | E | 4 | 0 | — |

---

## 10. API Reference — Multi-Agent Endpoints

All endpoints are mounted on the Stronghold gateway (default port 8443,
HTTP/2 over post-quantum TLS). Authentication is via `Authorization:
Bearer <agent_token>` (tenant-scoped, TTL'd) unless otherwise noted.
Agent tokens are minted by `auth::mint_agent_token` and verified by
`auth::verify_agent_token`.

### 10.1 Task Lifecycle

| Method | Path | Handler | Purpose |
|---|---|---|---|
| `POST` | `/agent/task` | `tasks::create_task` | Create a new queued task |
| `GET` | `/agent/task/:id` | `tasks::get_task` | Fetch a task's status + details |
| `POST` | `/agent/task/:id/result` | `tasks::submit_result` | Submit a task's execution result (`exit_code` 0 → `completed`, non-zero → `failed`) |
| `GET` | `/agent/task/:id/stream` | `tasks::stream_task` | SSE stream of task status updates |
| `POST` | `/agent/task/:id/progress` | `tasks::submit_progress` | Submit a mid-task progress report |
| `POST` | `/agent/task/:id/reflexion` | `tasks::submit_reflexion` | Submit a post-task reflexion |
| `GET` | `/agent/task/:id/reflexion` | `tasks::get_reflexion` | Retrieve a task's reflexion |
| `GET` | `/agent/reflexions` | `tasks::list_reflexions` | List recent reflexions (tenant-scoped, `?limit=` up to 100) |

**`POST /agent/task` body:**

```json
{
  "instruction": "Fix the auth token expiry bug in src/auth.rs",
  "image": "stronghold/rust-nightly:latest",
  "ttl_secs": 1800,
  "context": { "task_id": "task_01HXYZ" },
  "parent_task_id": null,
  "workflow_run_id": "wfr_01HXYZ",
  "role": "coder"
}
```

If `role` is set, the role's `system_prompt` is looked up in
`agent_roles` (tenant-scoped) and snapshotted into the task's `spec`
JSON as `role_system_prompt`. Role lookup is best-effort — a missing
role row is logged but doesn't fail task creation.

**`POST /agent/task/:id/result` body:**

```json
{
  "exit_code": 0,
  "stdout": "...",
  "stderr": "...",
  "summary": "All tests passed.",
  "artifacts": [{ "path": "target/release/binary" }]
}
```

### 10.2 Workflows

| Method | Path | Handler | Purpose |
|---|---|---|---|
| `POST` | `/workflow` | `workflows::create_workflow` | Define a new workflow (`status = draft`) |
| `GET` | `/workflow/:id` | `workflows::get_workflow` | Fetch a workflow definition |
| `GET` | `/workflow` | `workflows::list_workflows` | List workflows for the caller's tenant |
| `POST` | `/workflow/:id/run` | `workflows::run_workflow` | Start a run; spawns the engine in the background |
| `GET` | `/workflow/run/:id` | `workflows::get_run` | Poll a run's status / step progress |

**`POST /workflow` body:**

```json
{
  "name": "ci-build",
  "dag": {
    "steps": [
      { "id": "build", "task": "cargo build --release" },
      { "id": "test", "task": "cargo test", "depends_on": ["build"] }
    ]
  }
}
```

The DAG JSON is stored verbatim — the engine parses it at run time.

**`POST /workflow/:id/run` response:**

```json
{ "run_id": "wfr_01HXYZ", "status": "running" }
```

The engine (`workflow::engine::execute`) is spawned via `tokio::spawn`
and runs independently of the HTTP request. The client polls
`GET /workflow/run/:id` for progress:

```json
{
  "id": "wfr_01HXYZ",
  "workflow_id": "wf_01HXYZ",
  "status": "running",
  "current_steps": ["implement"],
  "completed_steps": ["plan"],
  "started_at": "2024-01-15T12:34:56",
  "finished_at": null
}
```

### 10.3 Agent Message Bus

| Method | Path | Handler | Purpose |
|---|---|---|---|
| `POST` | `/agent/:machine_id/messages` | `messages::post_message` | Post a message on a channel |
| `GET` | `/agent/:machine_id/messages` | `messages::poll_messages` | Poll for messages (`?channel=&since=`) |
| `GET` | `/agent/:machine_id/messages/stream` | `messages::stream_messages` | SSE stream of new messages |

These endpoints use the `connect_token` (issued at ORDER time, hashed
via SHA-256 → `machines.connect_token_hash`) via the `?token=` query
parameter — same pattern as the PTY and exec endpoints.

**`POST /agent/:machine_id/messages` body:**

```json
{
  "to": null,
  "channel": "workflow-run-wfr_01HXYZ",
  "body": { "type": "progress", "task_id": "task_01HXYZ", "status": "on_track" }
}
```

`to: null` means broadcast (any machine polling the channel receives
it).

### 10.4 Oracle (Q&A)

| Method | Path | Handler | Purpose |
|---|---|---|---|
| `POST` | `/agent/:machine_id/oracle` | `oracle::ask_oracle` | Ask the oracle a question |
| `GET` | `/agent/:machine_id/oracle/:question_id` | `oracle::get_answer` | Poll for the oracle's answer |

**`POST /agent/:machine_id/oracle` body:**

```json
{
  "question": "Where is the token validation logic?",
  "context": { "task_id": "task_01HXYZ" }
}
```

**Response:**

```json
{ "question_id": "oq_01HXYZ", "status": "queued" }
```

The question is posted on channel `oracle-<machine_id>`. The caller
polls `GET /agent/:machine_id/oracle/:question_id` until
`status == "answered"`.

### 10.5 Facilitator (Disagreement Mediation)

| Method | Path | Handler | Purpose |
|---|---|---|---|
| `POST` | `/agent/:machine_id/disagreement` | `facilitator::submit_disagreement` | Submit a disagreement for mediation |
| `GET` | `/agent/:machine_id/disagreement/:id` | `facilitator::get_decision` | Poll for the facilitator's decision |

**`POST /agent/:machine_id/disagreement` body:**

```json
{
  "task_id": "task_01HXYZ",
  "issue": "PR #42 should be merged despite failing lint",
  "coder_argument": "The lint rule is overly strict here.",
  "reviewer_argument": "The lint rule exists for a reason; fix the code.",
  "context": { "pr_url": "https://github.com/.../pull/42" }
}
```

The disagreement is recorded in the `disagreements` table and announced
on channel `facilitator-<workflow_run_id>` (or
`facilitator-task-<task_id>` / `facilitator-machine-<machine_id>` as
fallbacks).

### 10.6 Roles & Constitution

| Method | Path | Handler | Purpose |
|---|---|---|---|
| `POST` | `/admin/roles` | `roles::create_role` | Create a role |
| `GET` | `/admin/roles?tenant=` | `roles::list_roles` | List roles for a tenant |
| `GET` | `/admin/roles/:id` | `roles::get_role` | Fetch a single role |
| `DELETE` | `/admin/roles/:id` | `roles::delete_role` | Delete a role |
| `POST` | `/admin/roles/seed` | `roles::seed_roles` | Seed the 9 default roles for a tenant |
| `GET` | `/admin/constitution` | `roles::get_constitution` | Return the 10 constitutional principles |

**`POST /admin/roles/seed` body:**

```json
{ "tenant_id": "tenant_01HXYZ" }
```

Seeds all 9 default roles (planner, coder, reviewer, tester, integrator,
watchdog, oracle, architect, facilitator) with their canonical system
prompts and tool permissions. Idempotent — existing roles are skipped.

### 10.7 Credential Vault

| Method | Path | Handler | Auth | Purpose |
|---|---|---|---|---|
| `POST` | `/admin/credentials` | `credentials::create_credential` | agent token | Store an encrypted credential |
| `GET` | `/admin/credentials` | `credentials::list_credentials` | agent token | List credentials (metadata only) |
| `GET` | `/admin/credentials/:id` | `credentials::get_credential` | agent token | Fetch a credential (metadata only) |
| `DELETE` | `/admin/credentials/:id` | `credentials::delete_credential` | agent token | Delete a credential |
| `POST` | `/admin/credentials/:id/rotate` | `credentials::rotate_credential` | agent token | Rotate a credential's value |
| `GET` | `/agent/:machine_id/credentials/:name` | `credentials::agent_get_credential` | connect token | Agent fetches a credential by name (returns decrypted value) |

Credentials are encrypted with AES-256-GCM using a per-tenant key
derived via HKDF-256 from the audit Ed25519 secret key + `tenant_id`.
The tenant key is never stored — it's derived on demand.

### 10.8 Watchdog (internal)

The watchdog monitoring loop (`watchdog::monitor::spawn_watchdog`) runs
as a background task in `serve()`. It is **not** driven by HTTP
endpoints — it polls the DB every 60 seconds. The data it produces is
queryable via standard SQL on the `watchdog_reports` and `ultimata`
tables.

The `issue_ultimatum` and `check_ultimatum_acknowledgment` functions
are `pub` so they can be invoked directly by the monitor loop (and by
tests). See `gateway/src/watchdog/ultimatum.rs` for the full API.

---

## 11. Testing

The multi-agent system is covered by three integration test files in
`gateway/tests/`:

| File | Tests | What it covers |
|---|---|---|
| `multi_agent_test.rs` | 1 | E2E workflow: tenant → credential → 4-step DAG → 4 tasks → completed → reflexion → audit chain |
| `template_test.rs` | 4 | All 12 templates: valid JSON, required fields, no dangling deps, no cycles, valid condition/parallel_with refs |
| `watchdog_test.rs` | 3 | Dedication scoring (off-task < 0.3), workaround detection (≥ 2 warnings), Level 1/2 ultimatum issuance, ACK detection |

Plus ~30 unit tests in `gateway/src/workflow/engine.rs` covering DAG
deserialization, dependency resolution, condition evaluation, and
topological ordering; 6 in `watchdog/dedication.rs`; 15 in
`watchdog/detector.rs`; 13 in `watchdog/ultimatum.rs`.

Run the full suite with:

```bash
cargo test --workspace --features no-sev-snp
```

---

## 12. References

- **ReAct** — Yao et al., "ReAct: Synergizing Reasoning and Acting in Language Models," ICLR 2023.
- **Reflexion** — Shinn et al., "Reflexion: Language Agents with Verbal Reinforcement Learning," NeurIPS 2023.
- **MetaGPT** — Hong et al., "MetaGPT: Meta Programming for Multi-Agent Collaborative Framework," ICLR 2024.
- **ChatDev** — Qian et al., "Communicative Agents for Software Development," ACL 2024.
- **Multi-agent Debate** — Du et al., "Improving Factuality and Reasoning in Language Models through Multiagent Debate," arXiv 2023.
- **Self-Refine** — Madaan et al., "Self-Refine: Iterative Refinement with Self-Feedback," NeurIPS 2023.
- **Plan-and-Solve** — Wang et al., "Plan-and-Solve Prompting," ACL 2023.
- **Constitutional AI** — Bai et al., "Constitutional AI: Harmlessness from AI Feedback," arXiv 2022.
- **CodeAct** — Wang et al., "Executable Code Actions Elicit Better LLM Agents," ICML 2024.
- **GPTLens** — Sun et al., "GPTLens: A Dual-Agent Framework for Smart Contract Vulnerability Detection," arXiv 2024.
- **Mixture of Experts** — Jacobs et al., "Adaptive Mixtures of Local Experts," Neural Computation 1991.
- **RAG** — Lewis et al., "Retrieval-Augmented Generation," NeurIPS 2020.
- **AutoTest** — Schäfer et al., "Empirically Evaluating LLMs' Role in Automated Test Generation," 2024.
