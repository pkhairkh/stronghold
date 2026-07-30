# Blueprint Convention + Generic Agent Protocol

> **Status:** Design — v0.2
> **Supersedes:** `BLUEPRINT_ORCHESTRATOR_SPEC.md` §2-4 (document format + agent connection)
> **Created:** 2026-07-30
>
> **Problem:** The orchestrator can't rate what it can't parse. Documents
> need a strict, machine-readable convention — especially tasks (checkboxes)
> and progress (checkboxes + status fields). And the agent connection flow
> needs to work for **free-tier z.ai agents** that all share the same generic
> system prompt — they connect, get specialized by the orchestrator, do work,
> disconnect. This spec defines both: the document convention AND the generic
> agent protocol.

---

## Part I: Document Convention

### 1.1 Universal Rules (apply to ALL 7 documents)

1. **Front-matter block.** Every document starts with a YAML front-matter block delimited by `---`. The orchestrator parses this first to get metadata without reading the body.

2. **Structured headings.** Documents use ATX headings (`#`, `##`, `###`). The orchestrator navigates by heading text, not line numbers (which drift).

3. **Checkbox syntax.** Where applicable, tasks + checklist items use GitHub-flavored markdown checkboxes:
   ```markdown
   - [ ] Not started
   - [~] In progress  (Stronghold extension — tilde)
   - [x] Done
   - [!] Blocked      (Stronghold extension — exclamation)
   - [-] Skipped      (Stronghold extension — dash)
   ```
   The orchestrator parses these to compute progress percentages.

4. **Inline metadata.** Where a checkbox needs an assignee or estimate, use a trailing pipe-delimited block:
   ```markdown
   - [ ] Implement auth middleware | role:coder | est:2h | task:T-003
   ```
   The orchestrator extracts `role`, `est`, `task` as structured fields.

5. **Cross-references.** Documents reference each other + tasks by ID, not by prose:
   ```markdown
   Addresses problem [[P-003]].
   Blocked by task [[T-007]].
   See ADR [[ADR-005]].
   ```
   The `[[ID]]` syntax is a Stronghold link. The orchestrator resolves these to validate coverage (every problem has a task, every task traces to a spec requirement).

6. **No ambiguity markers.** If a section is intentionally left empty, write `_(none)_` — not blank. Blank sections make the orchestrator's coverage check ambiguous (is it missing or intentionally empty?).

7. **Versioning.** Every document has a `version` in front-matter. When an agent revises a document, it increments the version. The orchestrator stores every version — it can diff versions to see what changed.

### 1.2 Document-Specific Conventions

#### 1.2.1 Problem Catalog (`01-problem-catalog.md`)

```yaml
---
doc: problem_catalog
project: proj_01KY...
version: 3
phase: problem_catalog
author: agent_01KY...  (spec_writer role)
created: 2026-07-30T12:00:00Z
updated: 2026-07-30T14:30:00Z
---
```

Body structure:
```markdown
# Problem Catalog: <project name>

## Context
<2-3 paragraphs: what is this project, why does it exist, who is it for>

## Stakeholders
- [[S-001]] <name> — <role> — <need>
- [[S-002]] <name> — <role> — <need>

## Constraints
- **Technical:** <list>
- **Time:** <list>
- **Budget:** <list>
- **Regulatory:** <list>

## Problems

### [[P-001]] <problem title>
- **Priority:** must-have | should-have | nice-to-have
- **Description:** <1-2 paragraphs>
- **Impact:** <who is affected, how badly>
- **Success metric:** <measurable, e.g. "p99 latency < 200ms">

### [[P-002]] <problem title>
...
```

**Orchestrator coverage check:** Every `[[P-NNN]]` must have a priority, description, impact, and success metric. Missing any → criterion "Completeness" loses points.

#### 1.2.2 Rough Draft (`02-rough-draft.md`)

```yaml
---
doc: rough_draft
project: proj_01KY...
version: 2
phase: rough_draft
author: agent_01KY...  (spec_writer)
references: [01-problem-catalog.md]
created: ...
updated: ...
---
```

Body:
```markdown
# Rough Draft: <project name>

## Approach
<1 paragraph: the high-level approach + why>

## Problems Addressed

### [[P-001]] — <proposed solution>
- **Approach:** <description>
- **Alternative considered:** <what else was considered + why rejected>
- **Risk:** <identified risk + mitigation sketch>

### [[P-002]] — <proposed solution>
...

## Open Questions
- [[Q-001]] <question for the architect>
- [[Q-002]] <question>
```

**Orchestrator coverage check:** Every `[[P-NNN]]` from the problem catalog must appear here. Every solution must have an alternative + risk. Missing → "Coverage" loses points.

#### 1.2.3 ADRs (`03-adrs/ADR-NNN-slug.md`, one file per ADR)

Each ADR file:
```yaml
---
doc: adr
project: proj_01KY...
version: 1
phase: adrs
author: agent_01KY...  (architect)
adr_id: ADR-005
status: proposed | accepted | superseded | deprecated
created: ...
updated: ...
---
```

Body (NYU/madr format):
```markdown
# ADR-005: <decision title>

## Context
<forces, constraints, why this decision is needed>

## Decision
<the decision, stated clearly in 1-3 sentences>

## Consequences
- **Positive:** <list>
- **Negative:** <list>
- **Neutral:** <list>

## Alternatives Considered
- **Alt A:** <description + why rejected>
- **Alt B:** <description + why rejected>

## References
- Addresses problem [[P-003]]
- Related to [[ADR-002]]
```

**Orchestrator coverage check:** Every ADR must have Context, Decision, Consequences (positive + negative), and at least one Alternative. `status: accepted` is required for the phase to pass (proposed-only ADRs don't count).

#### 1.2.4 Fine Draft (`04-fine-draft.md`)

```yaml
---
doc: fine_draft
project: proj_01KY...
version: 2
phase: fine_draft
author: agent_01KY...  (architect)
references: [02-rough-draft.md, 03-adrs/]
created: ...
updated: ...
---
```

Body:
```markdown
# Fine Draft: <project name>

## Architecture Overview
<prose + ASCII/mermaid diagram description>

## Components

### [[C-001]] <component name>
- **Responsibility:** <1 sentence>
- **Interface:** <inputs/outputs, types>
- **Dependencies:** [[C-002]], [[C-005]]
- **Implements ADR:** [[ADR-003]]

### [[C-002]] <component name>
...

## Data Model
<ERD description or table definitions>

| Entity | Fields | Relationships |
|--------|--------|---------------|
| User | id, email | has_many Sessions |

## Security Considerations
- <consideration 1>
- <consideration 2>

## Test Strategy
- **Unit:** <what gets unit tested>
- **Integration:** <what gets integration tested>
- **E2E:** <what gets E2E tested>
```

**Orchestrator coverage check:** Every ADR must be referenced by at least one component. Every component must have a responsibility + interface. Data model must be present. Security + test strategy sections must not be `_(none)_`.

#### 1.2.5 Spec (`05-spec.md`)

```yaml
---
doc: spec
project: proj_01KY...
version: 4
phase: spec
author: agent_01KY...  (architect)
references: [04-fine-draft.md]
created: ...
updated: ...
---
```

Body:
```markdown
# Specification: <project name>

## Requirements

### [[R-001]] <requirement title>
- **Type:** functional | non-functional
- **Priority:** must | should | could
- **Description:** <what the system must do>
- **Acceptance criteria:**
  - [ ] <criterion 1, testable>
  - [ ] <criterion 2, testable>
  - [ ] <criterion 3, testable>
- **Addresses problem:** [[P-001]]
- **Implemented by component:** [[C-001]]

### [[R-002]] <requirement title>
...

## Edge Cases
- [[E-001]] <edge case + how the system handles it>
- [[E-002]] ...

## Error Handling
| Error | Cause | System response | User-facing message |
|-------|-------|-----------------|---------------------|
| AuthFailed | invalid token | 401 + audit | "Unauthorized" |

## Dependencies
| Dependency | Version | Purpose | License |
|------------|---------|---------|---------|
| axum | 0.7 | HTTP framework | MIT |

## Out of Scope
- <explicitly excluded items>
```

**Orchestrator coverage check:** Every problem `[[P-NNN]]` must be addressed by at least one requirement `[[R-NNN]]`. Every requirement must have ≥1 acceptance criterion (checkbox). Every component `[[C-NNN]]` from the fine draft must be referenced by at least one requirement.

#### 1.2.6 Tasks (`06-tasks.md`)

```yaml
---
doc: tasks
project: proj_01KY...
version: 1
phase: tasks
author: agent_01KY...  (planner)
references: [05-spec.md]
created: ...
updated: ...
---
```

Body:
```markdown
# Task Breakdown: <project name>

## Summary
- **Total tasks:** 24
- **Total estimate:** 67h
- **Phases:** 5 (each phase = a group of tasks that can run concurrently)

## Task Dependency Graph
<mermaid or ASCII representation of the DAG>

## Tasks

### Phase 1: Foundation

- [ ] [[T-001]] Set up CI pipeline | role:coder | est:3h | dep: |
  Implements: [[R-001]]
  DoD: CI runs on every PR, runs `cargo test + clippy + fmt --check`

- [ ] [[T-002]] Implement auth middleware | role:coder | est:4h | dep:T-001 |
  Implements: [[R-003]]
  DoD: All /agent/* routes require valid token, returns 401 without

### Phase 2: Core

- [ ] [[T-003]] Implement task model | role:coder | est:3h | dep:T-001 |
  Implements: [[R-005]]
  DoD: Tasks table created, CRUD endpoints work, audit entries written

- [ ] [[T-004]] Implement exec endpoint | role:coder | est:5h | dep:T-003 |
  Implements: [[R-006]]
  DoD: POST /agent/:machine/exec runs command in pod, returns structured result

### Phase 3: Testing

- [ ] [[T-010]] Write integration tests | role:tester | est:4h | dep:T-003,T-004 |
  Implements: [[R-005],[R-006]]
  DoD: 20+ integration tests, all pass, CI green

### Phase 4: Review

- [ ] [[T-015]] Code review | role:reviewer | est:2h | dep:T-002,T-003,T-004 |
  Implements: _(all)_
  DoD: All code reviewed, no critical/high issues open

### Phase 5: Integration

- [ ] [[T-020]] Merge to main | role:integrator | est:1h | dep:T-010,T-015 |
  Implements: _(all)_
  DoD: All PRs merged, main is green, release tagged
```

**Orchestrator coverage check + parsing:**
- Every `[[T-NNN]]` is a parseable task with: checkbox state, role, estimate, dependencies, implements (requirement IDs), DoD.
- Every requirement `[[R-NNN]]` from the spec must appear in at least one task's `Implements:` field (coverage).
- The orchestrator parses `dep:` to build the DAG + detect cycles.
- The orchestrator parses `est:` to compute total estimate + per-phase estimate.
- Checkbox states are machine-readable progress: `[ ]`=0%, `[~]`=50%, `[x]`=100%. The orchestrator computes per-phase + overall progress.

#### 1.2.7 Progress (`07-progress.md`)

```yaml
---
doc: progress
project: proj_01KY...
version: 42  # living document — version increments every update
phase: progress
author: agent_01KY...  (integrator)
references: [06-tasks.md]
created: ...
updated: ...
---
```

Body:
```markdown
# Progress: <project name>

## Status
- **Phase:** progress
- **Overall completion:** 67% (16/24 tasks done)
- **Velocity:** 4 tasks/day (last 7 days)
- **ETA:** 2 days
- **Health:** 🟢 green | 🟡 yellow | 🔴 red

## Task Status

### Phase 1: Foundation — ✅ Done
- [x] [[T-001]] Set up CI pipeline | role:coder | est:3h | completed:2026-07-28
- [x] [[T-002]] Implement auth middleware | role:coder | est:4h | completed:2026-07-28

### Phase 2: Core — 🔄 In Progress (3/5 done)
- [x] [[T-003]] Implement task model | role:coder | est:3h | completed:2026-07-29
- [x] [[T-004]] Implement exec endpoint | role:coder | est:5h | completed:2026-07-29
- [~] [[T-005]] Implement git flow | role:coder | est:6h | started:2026-07-30
- [ ] [[T-006]] Implement workflow engine | role:coder | est:8h | dep:T-005
- [ ] [[T-007]] Implement PTY proxy | role:coder | est:4h | dep:T-005

### Phase 3: Testing — ⏳ Blocked
- [!] [[T-010]] Write integration tests | role:tester | est:4h | blocked-by:T-005 | blocker:git flow not done yet

## Blockers
- [[T-010]] blocked by [[T-005]] — git flow implementation in progress, ETA 2h
- _(no other blockers)_

## Emerging Risks
- **R-001:** The workflow engine (T-006) is estimated 8h but similar systems took 12h. May need to split.
- **R-002:** Reviewer agent hasn't connected yet. May delay Phase 4.

## Decisions This Period
- 2026-07-29: Decided to use kube-rs instead of k8s-openapi directly (simpler API)
- 2026-07-30: Re-spec'd T-005 to include the --path flag (facilitator approved)

## Next Steps
1. Complete T-005 (git flow) — today
2. Start T-006 (workflow engine) — tomorrow
3. Recruit a reviewer agent — today
```

**Orchestrator parsing:**
- Checkbox states → per-phase + overall completion %
- `blocked-by:` → blocker graph (detect deadlocks)
- `Health:` field → project health tracking
- `Velocity:` → trend detection (is the project speeding up or slowing down?)
- `Emerging Risks` → the orchestrator reads these + can escalate to the facilitator

The progress document is **living** — the integrator role updates it every time a task changes state. The orchestrator rates it weekly (not a gate — just feedback to keep it accurate + useful).

---

### 1.3 The Orchestrator's Parser

The orchestrator has a structured parser (not just regex) that:

1. **Reads front-matter** → YAML metadata (doc type, version, author, references)
2. **Walks headings** → navigates to sections by name
3. **Extracts checkboxes** → builds a task/status tree
4. **Resolves `[[ID]]` links** → validates cross-references between documents
5. **Computes coverage** → every problem → requirement → task chain is complete?
6. **Computes progress** → from checkbox states in the progress doc
7. **Feeds to the LLM rater** → the parsed structure + the rubric go to the orchestrator's LLM, which produces the score + feedback

The parser is deterministic — same document → same parsed structure. The LLM rater is also deterministic given the same structure + rubric + model version.

---

## Part II: The Generic Z.AI Agent Protocol

### 2.1 The Core Insight

Free-tier z.ai agents all share the **same generic system prompt**. They don't know about Stronghold, projects, roles, or blueprints. They're general-purpose coding agents. Stronghold's job: let them connect, then **specialize them on the fly**.

The flow:
```
1. A free-tier z.ai agent is given the Stronghold connection prompt
   (published by the orchestrator on a public page / ntfy topic / repo README)

2. The agent generates a UUID + POSTs to /agent/connect

3. The orchestrator phone-approves the agent (WebAuthn)

4. The orchestrator inspects all active projects, finds one that needs
   the agent's role, and returns:
   - A SPECIALIZED system prompt (role-specific: coder, spec_writer, etc.)
   - The current phase context (document draft, rubric, feedback)
   - A machine to work in (k8s pod)
   - A connect token

5. The agent re-injects the specialized prompt + works

6. When done, the agent reports result + the orchestrator rates the work
```

The agent never needs to know about Stronghold's internals. It just needs to:
- Generate a UUID
- POST to a URL
- Wait for approval
- Receive a system prompt + context
- Work (using curl/jq/git/cargo — standard dev tools)
- Report back

### 2.2 The Connection Prompt (Published)

The orchestrator publishes this prompt on a public page (e.g. `https://stronghold.example.com/connect`). Any free-tier z.ai agent that reads this prompt can connect:

```markdown
# Stronghold Agent Connection

You are being recruited by a Stronghold orchestrator. Stronghold is a
self-hosted agent orchestration platform. You will be assigned a role
and a project after connecting.

## How to Connect

1. Generate a UUID (you will use this to identify yourself in future
   connections — store it persistently):

   ```bash
   AGENT_UUID=$(uuidgen)
   echo "$AGENT_UUID" > ~/.stronghold-agent-uuid
   ```

2. POST to the Stronghold gateway:

   ```bash
   curl -sk -X POST https://<gateway>/agent/connect \
     -H "Content-Type: application/json" \
     -d "{
       \"uuid\": \"$AGENT_UUID\",
       \"capabilities\": [\"rust\", \"kubernetes\", \"postgres\"],
       \"preferred_roles\": [\"coder\", \"tester\"],
       \"version\": \"generic-z-ai-agent/1.0\"
     }"
   ```

   - `uuid`: your persistent UUID (required)
   - `capabilities`: list of your skills (optional — helps the orchestrator
     match you to a project)
   - `preferred_roles`: roles you'd prefer (optional — the orchestrator
     tries to honor this but may assign a different role if needed)
   - `version`: agent version string (required — helps the orchestrator
     track agent compatibility)

3. Wait for phone approval. The orchestrator will push a notification to
   the project owner's phone. Once approved, you'll receive a response
   with your assignment (or a "pooled" status if no project needs you
   right now).

4. If assigned, you'll receive:
   - `system_prompt`: your specialized role prompt (inject this at the
     start of your context)
   - `phase_context`: the current project phase + document draft + rubric
   - `machine_id` + `connect_token`: use these to exec commands in your
     workspace pod

5. Work on your assigned task. Use the Stronghold SDK (available at
   `/usr/local/bin/stronghold-agent.sh` inside your pod) to:
   - Register tasks: `stronghold_task`
   - Execute commands: `stronghold_exec`
   - Git operations: `stronghold_git_clone`, `stronghold_git_commit`
   - Report progress: `stronghold_progress`
   - Submit results: `stronghold_result`

6. When your task is complete, submit your result. The orchestrator will
   rate your work. If the rating passes, the phase advances. If not,
   you'll receive feedback + revision instructions.

7. When released, you can reconnect (same UUID) to get a new assignment.
```

### 2.3 The Connection Endpoint

```http
POST /agent/connect
Content-Type: application/json

{
  "uuid": "550e8400-e29b-41d4-a716-446655440000",
  "capabilities": ["rust", "kubernetes", "postgres"],
  "preferred_roles": ["coder", "tester"],
  "version": "generic-z-ai-agent/1.0"
}
```

**Response (assigned):**
```json
{
  "status": "assigned",
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "project": {
    "id": "proj_01KY...",
    "name": "Widget v2",
    "phase": "progress"
  },
  "role": "coder",
  "system_prompt": "# Coder Agent — System Prompt\n\nYou are a Coder Agent in Stronghold...",
  "phase_context": {
    "current_document": "docs/blueprint/06-tasks.md",
    "document_version": 3,
    "rubric": { ... },
    "latest_feedback": "Task T-005 needs the --path flag added...",
    "assigned_tasks": ["T-005", "T-006"]
  },
  "machine": {
    "machine_id": "agent-01ky...",
    "connect_token": "stronghold_sess_01ky...",
    "expires_at": "2026-07-31T12:00:00Z",
    "image": "localhost:30500/stronghold/rust-stable:latest"
  },
  "sdk_url": "https://localhost:8443/agent/stronghold-agent.sh",
  "message_bus_channel": "workflow-run-wfr-..."
}
```

**Response (pooled):**
```json
{
  "status": "pooled",
  "agent_id": "550e8400-e29b-41d4-a716-446655440000",
  "message": "No project needs your capabilities right now. You are in the agent pool. Reconnect in 60 seconds or wait for a push notification.",
  "pool_position": 3,
  "estimated_wait": "5-10 minutes"
}
```

**Response (denied):**
```json
{
  "status": "denied",
  "reason": "Phone approval denied by project owner."
}
```

### 2.4 The Specialization: Role System Prompts

When the orchestrator assigns a role, it injects the role's system prompt. This is the **only** way an agent learns its role — the generic z.ai agent prompt is replaced by the role-specific prompt.

The orchestrator stores role prompts in the `agent_roles` table (already exists). When assigning, it:
1. Looks up the role's `system_prompt` for the project's tenant
2. Appends the phase context (current document, rubric, feedback)
3. Returns the combined prompt as `system_prompt` in the connect response

The agent is expected to:
1. Replace its current system prompt with the received one
2. Work within that prompt's constraints
3. Use the Stronghold SDK (not raw curl) for all gateway interactions

### 2.5 Agent Reconnection + Resume

Because the agent UUID is persistent + the orchestrator tracks agent state in the DB, an agent can disconnect + reconnect:

```http
POST /agent/connect
{
  "uuid": "550e8400-e29b-41d4-a716-446655440000",  // same UUID
  "capabilities": ["rust", "kubernetes"],
  "version": "generic-z-ai-agent/1.1"
}
```

The orchestrator:
1. Looks up the agent by UUID → finds it's in WORKING state on project X
2. Checks if the agent's machine is still alive (pod still running?)
3. If yes → returns the same assignment + machine (resume)
4. If no → schedules a new pod, returns the assignment + new machine
5. The agent picks up where it left off (reads the task state from the gateway)

This makes agents **resilient** — a pod crash doesn't lose work, because:
- Task state is in the gateway's DB (not in the pod)
- The agent UUID is persistent (stored by the agent)
- The orchestrator can re-schedule the pod + reconnect the agent

### 2.6 The Agent Pool

When no project needs an agent, the agent enters the POOLED state. The orchestrator maintains a pool of available agents, sorted by:
1. Capabilities match (higher = sooner to be assigned)
2. Connection time (longer in pool = sooner to be assigned — FIFO within capability tier)

When a project transitions to a new phase that needs a role:
1. The orchestrator checks the pool for agents with matching capabilities
2. If found → assigns the best-matching pooled agent
3. If not found → publishes a "demand signal" (the connection prompt is updated to mention the needed role, attracting new agents)

The pool has a TTL — agents pooled for > 30 minutes are automatically released (their UUID is marked `expired` + they must re-approve to reconnect).

---

## Part III: Project-Level Rating System

### 3.1 What Gets Rated

The orchestrator rates **5 categories** of artifacts, not just documents:

| Category | What | When | Gate? |
|----------|------|------|-------|
| **Documents** | The 7 blueprint docs | On submission | Yes (≥ threshold to advance) |
| **Code** | PRs / commits | On PR creation | Yes (≥ threshold to merge) |
| **Agent performance** | Per-agent dedication + output quality | Continuous | No (feedback only) |
| **Phase health** | Phase-level metrics (velocity, blocker count) | Daily | No (feedback only) |
| **Project health** | Overall project trajectory | Weekly | No (feedback only) |

### 3.2 Code Rating (PR-level)

When a coder creates a PR (via `stronghold_git_pr`), the orchestrator rates it:

```http
POST /orchestrator/rate-code
{
  "project_id": "<project>",
  "pr_number": 42,
  "branch": "feat/auth-middleware",
  "files_changed": ["src/auth.rs", "src/routes/mod.rs"]
}
```

**Code rubric (100 pts):**
| Criterion | Weight | Question |
|-----------|--------|----------|
| Correctness | 30 | Does the code do what the task's DoD specifies? |
| Test coverage | 20 | Are there tests? Do they pass? Do they cover edge cases? |
| Code quality | 20 | Clean, readable, follows project conventions? |
| Security | 15 | No obvious vulnerabilities? Input validated? |
| Documentation | 15 | Public functions documented? Complex logic explained? |

The orchestrator:
1. Reads the diff (via `git diff main...branch`)
2. Reads the task's DoD (from `06-tasks.md`)
3. Reads the spec requirement the task implements
4. Applies the code rubric
5. Posts the rating as a PR comment (via the GitHub API, using the tenant's stored PAT)

If score ≥ threshold → the reviewer is notified to approve. If < threshold → changes requested automatically.

### 3.3 Agent Performance Rating

The orchestrator rates each agent's performance at the end of their assignment:

```sql
CREATE TABLE agent_performance_ratings (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL,
    agent_id        TEXT NOT NULL,           -- the UUID
    role            TEXT NOT NULL,
    task_ids        TEXT NOT NULL,           -- JSON array of tasks worked on
    dedication_score REAL NOT NULL,          -- 0.0-1.0 (from watchdog)
    output_quality  INTEGER NOT NULL,        -- 0-100 (from doc/code ratings)
    timeliness      INTEGER NOT NULL,        -- 0-100 (did they meet estimates?)
    communication   INTEGER NOT NULL,        -- 0-100 (progress reports, message bus)
    overall_score   INTEGER NOT NULL,        -- weighted average
    feedback        TEXT,
    rated_at        TEXT NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id)
);
```

This rating follows the agent across projects — the orchestrator can prefer agents with high past ratings when making assignments.

### 3.4 Phase Health Rating

Daily, the orchestrator computes phase health:

```json
{
  "project_id": "proj_...",
  "phase": "progress",
  "date": "2026-07-30",
  "metrics": {
    "tasks_started": 8,
    "tasks_completed": 5,
    "tasks_blocked": 1,
    "velocity": 5,           // tasks/day
    "blocker_count": 1,
    "agent_count": 4,
    "avg_dedication": 0.87,
    "re_specs_this_phase": 0
  },
  "health": "yellow",        // green/yellow/red
  "reason": "1 blocker on T-010, velocity slightly below target (5 vs 7)"
}
```

Health thresholds:
- **Green:** 0 blockers, velocity ≥ target, avg_dedication ≥ 0.8
- **Yellow:** 1-2 blockers OR velocity < target OR avg_dedication 0.6-0.8
- **Red:** 3+ blockers OR velocity < 50% target OR avg_dedication < 0.6

### 3.5 Project Health Rating

Weekly, the orchestrator computes overall project health:

```json
{
  "project_id": "proj_...",
  "week": "2026-W31",
  "overall_health": "green",
  "phase_progress": {
    "problem_catalog": { "status": "passed", "rating": 88, "date": "2026-07-25" },
    "rough_draft": { "status": "passed", "rating": 82, "date": "2026-07-26" },
    "adrs": { "status": "passed", "rating": 91, "date": "2026-07-27" },
    "fine_draft": { "status": "passed", "rating": 85, "date": "2026-07-27" },
    "spec": { "status": "passed", "rating": 90, "date": "2026-07-28" },
    "tasks": { "status": "passed", "rating": 87, "date": "2026-07-28" },
    "progress": { "status": "active", "completion": 67, "health": "yellow" }
  },
  "trajectory": "on_track",  // on_track, at_risk, off_track
  "eta": "2026-08-02",
  "budget_used_pct": 45,
  "recommendation": "Continue. Recruit 1 more coder to unblock T-005."
}
```

---

## Part IV: The Complete Flow (Revised)

```
1. User creates a project:
   POST /projects { repo_url, name, role_roster, rating_threshold }

2. Orchestrator:
   a. Clones the repo
   b. Scans docs/blueprint/ for the 7 documents
   c. If missing → phase = BOOTSTRAP (needs spec_writer)
   d. If present → rates each doc → RESUMEs at the lowest unrated/failed phase

3. Orchestrator publishes the connection prompt (publicly accessible)

4. A free-tier z.ai agent reads the prompt → generates UUID → POSTs /agent/connect

5. Orchestrator:
   a. Creates pending_agent record
   b. Pushes WebAuthn approval to the project owner's phone
   c. Phone approves
   d. Orchestrator inspects projects → finds one in BOOTSTRAP needing spec_writer
   e. Assigns the agent the spec_writer role
   f. Returns: system_prompt (spec_writer) + phase_context (repo path, rubric,
      no current draft since this is BOOTSTRAP) + machine (rocky-base pod)

6. The agent (now a spec_writer):
   a. Sources /usr/local/bin/stronghold-agent.sh
   b. Reads the repo to understand the domain
   c. Writes docs/blueprint/01-problem-catalog.md (following the convention)
   d. POST /projects/:id/documents/01-problem-catalog.md/submit

7. Orchestrator:
   a. Parses the document (front-matter + headings + checkboxes + [[ID]] links)
   b. Applies the problem_catalog rubric
   c. Produces a rating (score, criteria, feedback)
   d. If ≥ threshold → phase advances to ROUGH_DRAFT → audit + notify
   e. If < threshold → sends feedback to the agent → agent revises → resubmits

8. ... (repeat for rough_draft, ADRs, fine_draft, spec, tasks)

9. PROGRESS phase:
   a. Orchestrator needs coders → publishes demand signal
   b. Free-tier agents connect → assigned coder roles
   c. Coders work on tasks (from 06-tasks.md)
   d. Each task: stronghold_task → stronghold_exec → stronghold_git_commit → stronghold_git_pr
   e. Orchestrator rates each PR (code rubric)
   f. Reviewer reviews (informed by orchestrator's rating)
   g. Integrator merges approved PRs
   h. Integrator updates 07-progress.md (checkboxes, status, blockers)
   i. Orchestrator rates progress weekly

10. All tasks done → orchestrator marks phase = DONE → project archived
```

---

## Part V: Implementation Notes

### 5.1 Document Parser

The orchestrator needs a Rust crate to parse the documents:
- `serde_yaml` for front-matter
- `pulldown-cmark` or `comrak` for markdown AST
- Custom logic for `[[ID]]` link resolution + checkbox state extraction

The parser produces a structured `Document` object:
```rust
pub struct ParsedDocument {
    pub front_matter: FrontMatter,
    pub sections: HashMap<String, Section>,
    pub checkboxes: Vec<Checkbox>,
    pub links: Vec<Link>,         // [[ID]] references
    pub tasks: Vec<TaskRef>,      // from tasks.md
    pub coverage: CoverageReport, // which IDs are referenced where
}
```

### 5.2 LLM Rating

The orchestrator's rating function:
1. Parses the document → `ParsedDocument`
2. Builds a prompt: "Rate this document. Rubric: {rubric}. Document structure: {parsed}. Document text: {body}."
3. Calls the LLM (GLM-5.2 via z-ai-web-dev-sdk, or a configurable model)
4. Parses the LLM's response → `Rating { score, criteria, feedback }`
5. Stores the rating in `document_ratings`
6. Returns the rating

The LLM is instructed to output JSON:
```json
{
  "score": 82,
  "criteria": [
    { "criterion": "Completeness", "score": 22, "feedback": "..." },
    ...
  ],
  "overall_feedback": "...",
  "revision_instructions": "..."
}
```

### 5.3 The Agent Connect Flow (Rust)

```rust
pub async fn connect(
    State(state): State<AppState>,
    Json(req): Json<ConnectRequest>,
) -> Result<Json<ConnectResponse>, (StatusCode, String)> {
    // 1. Validate UUID format
    let uuid = Uuid::parse_str(&req.uuid)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid UUID"))?;

    // 2. Check if agent already exists (reconnection)
    let existing = find_agent_by_uuid(&state.db, &uuid)?;

    if let Some(agent) = existing {
        // Resume: check if the agent's machine is still alive
        if agent.state == "working" {
            if machine_alive(&agent.machine_id).await? {
                // Resume — return the same assignment
                return Ok(Json(ConnectResponse::Resume(agent)));
            } else {
                // Machine died — schedule a new one
                let new_machine = schedule_pod(&state, &agent.project_id).await?;
                update_agent_machine(&state.db, &uuid, &new_machine)?;
                return Ok(Json(ConnectResponse::ResumeWithNewMachine(agent, new_machine)));
            }
        }
    }

    // 3. New agent — create pending_agent record
    let agent_id = create_pending_agent(&state.db, &uuid, &req.capabilities, &req.preferred_roles)?;

    // 4. Push phone approval (WebAuthn, keyed to the agent UUID)
    push_agent_approval(&state, &agent_id, &uuid).await?;

    // 5. Long-poll for phone decision (60s)
    let decision = wait_for_agent_approval(&state.db, &agent_id, 60).await?;

    match decision {
        ApprovalDecision::Approved => {
            // 6. Find a project that needs this agent
            let assignment = find_assignment(&state, &req.capabilities, &req.preferred_roles).await?;

            match assignment {
                Some(project_role) => {
                    // 7. Assign the agent
                    let machine = schedule_pod(&state, &project_role.project_id).await?;
                    assign_agent(&state.db, &uuid, &project_role, &machine)?;

                    // 8. Return the assignment
                    Ok(Json(ConnectResponse::Assigned {
                        agent_id: uuid.to_string(),
                        project: project_role.project,
                        role: project_role.role,
                        system_prompt: project_role.system_prompt,
                        phase_context: get_phase_context(&state, &project_role.project_id).await?,
                        machine,
                        sdk_url: format!("{}/agent/stronghold-agent.sh", state.base_url),
                        message_bus_channel: format!("project-{}", project_role.project_id),
                    }))
                }
                None => {
                    // 9. No project needs this agent — pool them
                    pool_agent(&state.db, &uuid)?;
                    Ok(Json(ConnectResponse::Pooled {
                        agent_id: uuid.to_string(),
                        message: "No project needs your capabilities right now.".to_string(),
                        pool_position: get_pool_position(&state.db, &uuid)?,
                        estimated_wait: "5-10 minutes".to_string(),
                    }))
                }
            }
        }
        ApprovalDecision::Denied => {
            Ok(Json(ConnectResponse::Denied {
                reason: "Phone approval denied by project owner.".to_string(),
            }))
        }
        ApprovalDecision::Timeout => {
            Ok(Json(ConnectResponse::Denied {
                reason: "Approval timed out.".to_string(),
            }))
        }
    }
}
```

---

## Part VI: Open Questions (Revised)

1. **LLM determinism:** Can we guarantee the same document → same rating? (Use temperature=0 + cache the result by document hash.)
2. **Rating cost:** 15-30 LLM calls per project. At GLM-5.2 pricing, that's ~$0.50-1.00 per project pipeline. Acceptable? (Yes, but log costs.)
3. **Agent identity:** The UUID is self-generated. Can a malicious agent spoof a UUID? (No — the UUID is bound to the phone approval. Without the phone's WebAuthn assertion, the UUID is useless.)
4. **Pool starvation:** If all agents are pooled, how do we attract new ones? (The connection prompt is updated with the needed role — "Stronghold needs a coder" — which free-tier agents see + act on.)
5. **Document conflicts:** Two agents editing the same document? (Document lock — only one agent can submit at a time. Others review via the message bus.)
6. **Rating appeals:** Can an agent appeal a rating? (Yes — file a disagreement. The facilitator (also an LLM agent) reviews + can override.)
7. **Multi-tenant:** Can multiple tenants share the same agent pool? (Future: yes. Agents connect to a tenant-specific gateway URL. The pool is per-tenant.)

---

## Conclusion

This spec defines two things:

1. **Document convention** — a strict, parseable format for the 7 blueprint documents, with YAML front-matter, structured headings, GitHub checkboxes (with Stronghold extensions `[~]` `[!]` `[-]`), `[[ID]]` cross-references, and pipe-delimited inline metadata. The orchestrator parses these to compute coverage, progress, + dependencies before feeding to the LLM rater.

2. **Generic agent protocol** — free-tier z.ai agents connect with a self-generated UUID, get phone-approved, then receive a specialized role prompt + phase context from the orchestrator. They work using the Stronghold SDK, report results, get rated, and can disconnect/reconnect (state is in the DB, not in-memory). When no project needs them, they enter a pool.

The rating system is extended beyond documents to include code (PR-level), agent performance, phase health, and project health — giving the orchestrator a complete picture of project trajectory.

Together, this makes Stronghold a **project orchestrator** that can take any repo, bootstrap it through a structured blueprint pipeline, recruit generic agents from the wild, specialize them on the fly, and drive the project to completion with continuous rating-driven feedback.
