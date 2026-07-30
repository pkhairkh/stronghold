# Stronghold Blueprint Orchestrator — Unified Architecture (v1.0)

> **Status:** Definitive design — v1.0
> **Supersedes:** `BLUEPRINT_ORCHESTRATOR_SPEC.md` (v0.1),
> `BLUEPRINT_CONVENTION_AND_AGENT_PROTOCOL.md` (v0.2),
> `REPROMPT_INJECTION_PROTOCOL.md` (v0.3)
> **Created:** 2026-07-30
>
> **What changed from v0.3:** This is a ground-up rewrite that addresses
> 10 maturity gaps in the prior specs. The prior specs defined the *what*
> (project state machine, document convention, reprompt injection). This
> spec defines the *how* — the complete runtime architecture, the
> orchestrator's own lifecycle, the agent economics model, the document
> conflict protocol, the parallel phase execution, the discovery mechanism,
> the rating calibration, the re-spec economics, and the human-in-the-loop.

---

## 0. Table of Contents

1. [System Architecture](#1-system-architecture)
2. [The Orchestrator — It's Also Stateless](#2-the-orchestrator--its-also-stateless)
3. [The Project Lifecycle — Concurrent, Not Linear](#3-the-project-lifecycle--concurrent-not-linear)
4. [Document Convention — Parseable + Conflict-Safe](#4-document-convention--parseable--conflict-safe)
5. [The Reprompt Injection — Multi-Agent, Multi-Channel](#5-the-reprompt-injection--multi-agent-multi-channel)
6. [Agent Economics — Pool, Priority, Bidding](#6-agent-economics--pool-priority-bidding)
7. [The Rating Pipeline — Not Just an LLM Call](#7-the-rating-pipeline--not-just-an-llm-call)
8. [Re-Spec Economics — Cost, Budget, Throttling](#8-re-spec-economics--cost-budget-throttling)
9. [Discovery — How Agents Find Stronghold](#9-discovery--how-agents-find-stronghold)
10. [Human-in-the-Loop — Owner Privileges](#10-human-in-the-loop--owner-privileges)
11. [The Bootstrap Prompt — Extended](#11-the-bootstrap-prompt--extended)
12. [Data Model — Complete](#12-data-model--complete)
13. [API Surface — Complete](#13-api-surface--complete)
14. [Implementation Waves](#14-implementation-waves)

---

## 1. System Architecture

### 1.1 The Three Layers

```
┌─────────────────────────────────────────────────────────────────────┐
│  LAYER 3: THE PROJECT PLANE                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐           │
│  │ Project A│  │ Project B│  │ Project C│  │ Project D│           │
│  │ (spec)   │  │ (progress)│ │(bootstrap)│  │ (done)   │           │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────────┘           │
│       │              │              │                                  │
├───────┼──────────────┼──────────────┼──────────────────────────────┤
│  LAYER 2: THE ORCHESTRATOR PLANE                                    │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────┐             │
│  │ Reprompt    │  │ Rating       │  │ Agent          │             │
│  │ Injector    │  │ Pipeline     │  │ Allocator      │             │
│  └─────────────┘  └──────────────┘  └────────────────┘             │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────┐             │
│  │ Document    │  │ Phase        │  │ Conflict       │             │
│  │ Parser      │  │ Scheduler    │  │ Resolver       │             │
│  └─────────────┘  └──────────────┘  └────────────────┘             │
├─────────────────────────────────────────────────────────────────────┤
│  LAYER 1: THE GATEWAY (existing Stronghold)                         │
│  ┌─────────┐ ┌──────┐ ┌───────┐ ┌──────┐ ┌──────┐ ┌──────────┐    │
│  │ /agent  │ │/phone│ │/admin │ │/wf   │ │/creds│ │ k3s sched │    │
│  └─────────┘ └──────┘ └───────┘ └──────┘ └──────┘ └──────────┘    │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │ SQLite (projects, agents, ratings, audit) + k3s (pods)     │   │
│  └─────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

- **Layer 1 (Gateway):** the existing Stronghold gateway — HTTP/WebSocket endpoints, k3s scheduler, crypto, audit log. Unchanged.
- **Layer 2 (Orchestrator Plane):** new services that run *inside* the gateway process (not separate pods). They're async background tasks + route handlers. State lives in the same SQLite DB.
- **Layer 3 (Project Plane):** logical projects, each with its own phase, documents, agents. Projects don't have their own processes — they're data structures managed by Layer 2.

### 1.2 Why the Orchestrator Is Inside the Gateway

The orchestrator needs:
- DB access (projects, agents, ratings)
- k3s access (schedule pods)
- Audit log access (write phase transitions, ratings)
- The crypto keys (sign audit entries)

Running it as a separate service would require replicating all this access. Instead, the orchestrator is a set of `tokio::spawn`'d background tasks + route handlers inside the gateway binary. It shares the gateway's `AppState`.

### 1.3 The Orchestrator Is NOT One Thing

"Orchestrator" is a role, not a service. The orchestrator plane is 6 services:

| Service | Job | Trigger |
|---------|-----|---------|
| RepromptInjector | Inject reprompts into agents | Turn start, heartbeat, phase change, feedback |
| RatingPipeline | Parse + rate documents/code | On document/PR submission |
| AgentAllocator | Match pooled agents to project role demand | On agent connect, on phase change |
| DocumentParser | Parse markdown → structured data | Called by RatingPipeline |
| PhaseScheduler | Determine which phases can run concurrently | On phase transition |
| ConflictResolver | Handle concurrent edits to the same document | On edit conflict |

Each service is independently testable + has its own audit trail.

---

## 2. The Orchestrator — It's Also Stateless

### 2.1 The Problem

The orchestrator uses an LLM to rate documents. But the LLM is also stateless — it doesn't remember that it rated project A's spec a 72 yesterday. Every rating call is a fresh turn.

If the orchestrator LLM forgets its past ratings, it can't:
- Detect drift (am I rating more harshly than last week?)
- Maintain consistency (I rated project A's spec a 72, so project B's similar spec should get ~72)
- Learn from human corrections (the owner overrode my 65 to an 80 — I should adjust)

### 2.2 The Solution: Orchestrator Memory via DB

The orchestrator LLM is stateless, but the **gateway** is not. The gateway stores every rating in `document_ratings`. Before the orchestrator LLM rates a new document, the gateway injects the orchestrator's "memory" into the LLM prompt:

```
## YOUR RATING HISTORY (last 20 ratings)
- 2026-07-28: problem_catalog for "Widget v2" → 72 (FAILED) → revised → 85 (PASSED)
- 2026-07-29: rough_draft for "Widget v2" → 88 (PASSED)
- 2026-07-29: spec for "API Gateway" → 65 (FAILED) → owner overrode to 80
- 2026-07-30: tasks for "Widget v2" → 91 (PASSED)
...

## CALIBRATION NOTES
- The owner flagged that you rated the "API Gateway" spec too harshly (65 → 80 override).
  Consider being more lenient on "alternative considerations" for specs that are
  otherwise complete.
- Your average rating over the last 20: 78.6. Target average: 75 (don't inflate).

## THE DOCUMENT TO RATE NOW
<parsed document structure + body>
```

This gives the orchestrator LLM:
- **Consistency:** it can see its past ratings for similar docs
- **Drift detection:** it can see its average + adjust
- **Human correction:** it can see where the owner disagreed + why

### 2.3 The Orchestrator's Own Reprompt

The orchestrator is itself an agent. It gets its own reprompt on every rating call:

```
## STRONGHOLD ORCHESTRATOR REPROMPT
### IDENTITY
You are the Stronghold Orchestrator. You rate documents + code.
You do NOT write code. You judge.

### CURRENT TASK
Rate: docs/blueprint/05-spec.md (version 4) for project "Widget v2"
Phase: spec
Rubric: Requirements 25, Acceptance criteria 25, Edge cases 20, Dependencies 15, Format 15

### YOUR HISTORY (last 20 ratings)
<rating history>

### CALIBRATION
<calibration notes>

### PARSED DOCUMENT
<structured parse: front-matter, sections, checkboxes, links, coverage>

### DOCUMENT BODY
<full markdown>

### INSTRUCTION
Produce a JSON rating: {score, criteria: [{criterion, score, feedback}], overall_feedback, revision_instructions}
Be consistent with your past ratings. Be calibrated to your average.
```

The orchestrator's reprompt is assembled by the `RatingPipeline` service from DB state + the parsed document.

### 2.4 The Orchestrator Is Multi-Tenant

Each tenant has their own orchestrator "instance" (logically — it's the same code, but the rating history + calibration is per-tenant). Tenant A's orchestrator might be calibrated differently than tenant B's. The `tenant_id` is part of every rating query.

---

## 3. The Project Lifecycle — Concurrent, Not Linear

### 3.1 The Problem with Linear Phases

The v0.1 spec defined a strict linear pipeline:
```
problem_catalog → rough_draft → ADRs → fine_draft → spec → tasks → progress
```

But real projects don't work this way. While the architect is writing ADRs, the spec_writer can start the rough draft. While the planner is decomposing tasks, early tasks can start. Forcing linearity wastes time + agent capacity.

### 3.2 The Concurrent Phase Model

Phases have **dependencies** (entry criteria), but multiple phases can be active simultaneously once their dependencies are met:

```
                    ┌──────────────┐
                    │ problem_catalog│
                    └──────┬───────┘
                           │ (rated ≥ threshold)
                    ┌──────┴───────┐
                    │ rough_draft  │
                    └──────┬───────┘
                           │ (rated ≥ threshold)
              ┌────────────┼────────────┐
              ▼            ▼            ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐
        │   ADRs   │ │fine_draft│ │  (wait)  │
        └────┬─────┘ └────┬─────┘ └──────────┘
             │             │
             └──────┬──────┘
                    ▼
              ┌──────────┐
              │   spec   │
              └────┬─────┘
                   │ (rated ≥ threshold)
              ┌────┴─────┐
              │  tasks   │
              └────┬─────┘
                   │ (rated ≥ threshold)
              ┌────┴─────────────────┐
              │ progress (concurrent)│
              │  ┌─────┐ ┌──────┐   │
              │  │code │ │ test │   │
              │  └─────┘ └──────┘   │
              │  ┌──────┐┌──────┐   │
              │  │review││integ │   │
              │  └──────┘└──────┘   │
              └─────────────────────┘
```

### 3.3 Phase Dependency Rules

| Phase | Depends on | Can run concurrently with |
|-------|-----------|--------------------------|
| `problem_catalog` | — | — |
| `rough_draft` | problem_catalog ≥ threshold | — |
| `adrs` | rough_draft ≥ threshold | fine_draft (if rough_draft covers architecture) |
| `fine_draft` | rough_draft ≥ threshold | adrs |
| `spec` | adrs ≥ threshold AND fine_draft ≥ threshold | — |
| `tasks` | spec ≥ threshold | — |
| `progress` | tasks ≥ threshold | — |

The `PhaseScheduler` service computes the set of active phases given the current state. When a phase's rating passes, the scheduler checks if new phases can start.

### 3.4 The Project State (Revised)

A project's state is no longer a single `phase` field — it's a set of active phases + their document ratings:

```sql
CREATE TABLE project_phase_states (
    project_id      TEXT NOT NULL,
    phase           TEXT NOT NULL,
    status          TEXT NOT NULL,   -- pending, active, passed, failed, skipped
    document_version INTEGER,        -- latest version submitted
    rating_id       TEXT,            -- latest rating
    started_at      TEXT,
    completed_at    TEXT,
    PRIMARY KEY (project_id, phase)
);
```

The project's "current phase" (for display) is the **lowest incomplete phase** — the one that's blocking the most downstream work.

---

## 4. Document Convention — Parseable + Conflict-Safe

### 4.1 The Convention (recap from v0.3, extended)

- **YAML front-matter**: doc type, project, version, author, references
- **ATX headings**: `#`, `##`, `###` — orchestrator navigates by heading text
- **Checkboxes**: `[ ]` `[~]` `[x]` `[!]` `[-]` with progress percentages
- **Cross-references**: `[[P-NNN]]`, `[[R-NNN]]`, `[[T-NNN]]`, `[[ADR-NNN]]`, `[[C-NNN]]`
- **Inline metadata**: `| role:coder | est:6h | dep:T-001 | implements:R-006 |`
- **Versioning**: every revision increments `version` in front-matter

### 4.2 The Document Lock Protocol

**Problem:** Two agents edit the same document simultaneously → conflict.

**Solution:** Document locks. Before an agent can edit a document, it must acquire a lock:

```http
POST /projects/:id/documents/:path/lock
{ "agent_uuid": "550e...", "expected_version": 3 }
```

- `expected_version`: the version the agent last read. If the current version ≠ expected, the lock is denied (someone else modified it — re-read first).
- The lock is held for 5 minutes (configurable). After that, it auto-expires.
- Only one agent can hold the lock at a time.
- The lock is released on `POST /projects/:id/documents/:path/submit` or `POST /projects/:id/documents/:path/unlock`.

```sql
CREATE TABLE document_locks (
    project_id      TEXT NOT NULL,
    document_path   TEXT NOT NULL,
    agent_uuid      TEXT NOT NULL,
    locked_version  INTEGER NOT NULL,
    locked_at       TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    PRIMARY KEY (project_id, document_path)
);
```

### 4.3 The Conflict Resolver

If an agent tries to submit a document without holding the lock, or with a stale version:

```json
{
  "error": "document_version_conflict",
  "current_version": 5,
  "your_version": 3,
  "current_author": "agent_998...",
  "current_locked_until": "2026-07-30T12:05:00Z",
  "resolution": "re_read_and_merge"
}
```

The agent must:
1. Re-read the current version
2. Merge its changes (or rebase on top)
3. Re-acquire the lock
4. Re-submit

For simple documents (problem catalog, rough draft), merge conflicts are rare because the spec_writer works alone. For the progress document (updated by the integrator), conflicts are more common — the integrator should use optimistic locking (read version N, submit version N+1, retry if someone else submitted N+1 first).

### 4.4 The Document Parser (Extended)

```rust
pub struct ParsedDocument {
    pub front_matter: FrontMatter,
    pub sections: HashMap<String, Section>,
    pub checkboxes: Vec<Checkbox>,
    pub links: Vec<CrossReference>,
    pub tasks: Vec<TaskRef>,
    pub coverage: CoverageReport,
    pub word_count: usize,
    pub section_count: usize,
    pub missing_sections: Vec<String>,   // required sections that are absent
    pub broken_links: Vec<String>,       // [[ID]] references that don't resolve
}

pub struct CoverageReport {
    pub problems_addressed: Vec<String>,     // P-NNN that have R-NNN
    pub problems_unaddressed: Vec<String>,   // P-NNN with no R-NNN
    pub requirements_implemented: Vec<String>,
    pub requirements_unimplemented: Vec<String>,
    pub tasks_without_requirement: Vec<String>,
    pub adrs_without_component: Vec<String>,
}
```

The `CoverageReport` is the **objective** part of the rating — the orchestrator LLM doesn't need to assess coverage (the parser does that). The LLM assesses **quality** (clarity, feasibility, etc.).

---

## 5. The Reprompt Injection — Multi-Agent, Multi-Channel

### 5.1 The Problem with Single-Agent Assumption

The v0.3 spec assumed one agent per machine. But the existing Stronghold multi-agent scenario has 9 agents sharing one pod. The reprompt injection must handle:
- Multiple agents on the same machine, each with their own reprompt stream
- Agents that are idle (waiting for a dependency) — they still need heartbeats
- Agents that are reassigned mid-task

### 5.2 The Agent-Channel Mapping

Each agent has a **logical channel** identified by its UUID, independent of which machine it's on:

```
Agent UUID: 550e8400-...
  ├── Channel: pty (if working interactively on machine A)
  ├── Channel: control (if working non-interactively)
  └── Channel: task (if fire-and-forget)
```

The `RepromptInjector` tracks each agent's current channel + routes reprompts accordingly. When an agent is reassigned to a new machine, the channel updates.

### 5.3 Multi-Agent on One Machine

When 9 agents share one pod (the multi-agent scenario), each agent gets its own reprompt stream via the **control WebSocket** (not the PTY — the PTY is shared). The orchestrator multiplexes:

```
Machine: agent-01ky... (shared by 9 agents)
  ├── Agent 550e... → control WS → reprompts for role:planner
  ├── Agent 998a... → control WS → reprompts for role:coder
  ├── Agent abc1... → control WS → reprompts for role:reviewer
  └── ...
```

Each agent's reprompt is independent — the planner's reprompt doesn't mention the coder's task (unless there's a coordination message). The message bus handles inter-agent communication; the reprompt handles agent-orchestrator communication.

### 5.4 The Reprompt Queue

Each agent has a reprompt queue (FIFO). When the orchestrator injects a reprompt, it goes into the queue. The agent processes reprompts in order:

```sql
CREATE TABLE reprompt_queue (
    id              TEXT PRIMARY KEY,          -- rq_<ULID>
    agent_uuid      TEXT NOT NULL,
    project_id      TEXT NOT NULL,
    trigger         TEXT NOT NULL,             -- turn_start, heartbeat, phase_change, feedback, message, reassign
    priority        INTEGER NOT NULL DEFAULT 0, -- 0=normal, 1=high, 2=urgent
    reprompt_json   TEXT NOT NULL,             -- the full reprompt block
    delivered_at    TEXT,                      -- when the agent consumed it
    created_at      TEXT NOT NULL
);
```

The agent's wrapper script (or the control WS handler) polls this queue + delivers reprompts to the LLM. High-priority reprompts (phase change, reassignment) preempt normal ones.

### 5.5 The Reprompt Composition

A reprompt is composed by the `RepromptInjector` from multiple sources:

```rust
pub fn compose_reprompt(agent: &Agent, project: &Project, trigger: RepromptTrigger) -> Reprompt {
    let mut block = String::new();

    // 1. IDENTITY
    block.push_str(&format!("## STRONGHOLD REPROMPT (turn {})\n", agent.turn_count));
    block.push_str(&format!("### IDENTITY\nYou are Stronghold agent {}.\n", agent.uuid));

    // 2. ROLE
    block.push_str(&format!("### ROLE\n{}\n", agent.role.system_prompt));

    // 3. PROJECT
    block.push_str(&format!("### PROJECT\nProject: {} (phase: {})\n", project.name, project.current_phase));

    // 4. TASK
    if let Some(task) = &agent.current_task {
        block.push_str(&format!("### TASK\nTask: [[{}]] {}\n", task.id, task.instruction));
        block.push_str(&format!("Implements: [[{}]]\n", task.implements));
        block.push_str(&format!("DoD: {}\n", task.dod));
    }

    // 5. CONTEXT
    block.push_str("### CONTEXT\n");
    block.push_str(&format!("Latest feedback: {}\n", agent.latest_feedback));
    block.push_str("Recent messages (last 5):\n");
    for msg in &agent.recent_messages {
        block.push_str(&format!("  [{}] {}: {}\n", msg.role, msg.kind, msg.summary));
    }

    // 6. INSTRUCTION (from the trigger)
    block.push_str(&format!("### INSTRUCTION\n{}\n", trigger.instruction()));

    // 7. SDK
    block.push_str(&format!("### SDK\nsource /usr/local/bin/stronghold-agent.sh\n"));
    block.push_str(&format!("export STRONGHOLD_URL={}\n", agent.gateway_url));
    block.push_str(&format!("export STRONGHOLD_TOKEN={}\n", agent.connect_token));

    // 8. CONSTRAINTS
    block.push_str("### CONSTRAINTS\n");
    for c in &agent.role.constraints {
        block.push_str(&format!("- {}\n", c));
    }

    Reprompt { block, priority: trigger.priority() }
}
```

---

## 6. Agent Economics — Pool, Priority, Bidding

### 6.1 The Problem

Free-tier z.ai agents are a finite, shared resource. When 10 projects need coders and only 3 coder-capable agents are in the pool, who gets them? The v0.2 spec said "FIFO within capability tier" — but that's unfair to high-priority projects + doesn't account for project urgency.

### 6.2 The Allocation Algorithm

The `AgentAllocator` service uses a **weighted priority** algorithm:

```python
def allocate_agent(agent):
    candidates = []
    for project in active_projects_needing_role(agent.capabilities):
        score = (
            project.priority * 0.40 +           # user-set priority (1-10)
            project.urgency * 0.25 +            # computed: blocked tasks, ETA risk
            project.stake * 0.20 +              # tenant-set: how important is this project?
            time_in_pool(agent) * 0.15          # agent fairness: longer pool = higher
        )
        candidates.append((project, score))
    candidates.sort(key=lambda x: x[1], reverse=True)
    return candidates[0][0] if candidates else None
```

### 6.3 Project Priority + Urgency

- **Priority** (1-10): set by the user at project creation. Higher = more important.
- **Urgency** (0-1): computed by the orchestrator:
  - Blocked tasks ratio (more blocked = more urgent)
  - ETA risk (ETA approaching + behind schedule = more urgent)
  - Phase staleness (in a phase > 7 days without progress = more urgent)
- **Stake** (1-10): set by the tenant per-project. Represents business value.

### 6.4 The Agent Pool

```sql
CREATE TABLE agent_pool (
    agent_uuid      TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    capabilities    TEXT NOT NULL,             -- JSON array
    preferred_roles TEXT NOT NULL,             -- JSON array
    pooled_at       TEXT NOT NULL,
    pool_expires_at TEXT NOT NULL,             -- auto-release after 30 min
    assignment_attempts INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);
```

### 6.5 Pool TTL + Fairness

- Agents pooled > 30 minutes are auto-released (their UUID is marked `expired`).
- An agent that's been assigned + released 3 times without completing a task is flagged `low_reliability` — future allocations deprioritize it.
- The allocator logs every allocation decision (which project, which agent, what score) for audit + fairness analysis.

### 6.6 Demand Signaling

When a project needs a role but no pooled agent matches, the orchestrator publishes a **demand signal**:

```http
GET /agent/demand
```

Returns:
```json
{
  "demand": [
    { "role": "coder", "projects": 3, "urgency": "high", "capabilities_needed": ["rust"] },
    { "role": "spec_writer", "projects": 1, "urgency": "medium", "capabilities_needed": [] }
  ]
}
```

The bootstrap prompt (§11) instructs agents to check `/agent/demand` before connecting — if their capabilities match a high-urgency demand, they should connect.

---

## 7. The Rating Pipeline — Not Just an LLM Call

### 7.1 The Pipeline

The v0.3 spec treated rating as a single LLM call. In reality, it's a 5-stage pipeline:

```
Document Submitted
       │
       ▼
┌──────────────┐
│ 1. PARSE     │  DocumentParser → ParsedDocument
└──────┬───────┘
       │
       ▼
┌──────────────┐
│ 2. OBJECTIVE │  Compute coverage, progress, missing sections,
│    METRICS   │  broken links, word count, checkbox states
└──────┬───────┘
       │
       ▼
┌──────────────┐
│ 3. LLM RATE  │  LlmRater → subjective assessment (clarity,
│              │  feasibility, quality) using rubric + history
└──────┬───────┘
       │
       ▼
┌──────────────┐
│ 4. AGGREGATE │  Combine objective + subjective → final score
└──────┬───────┘
       │
       ▼
┌──────────────┐
│ 5. THRESHOLD │  score ≥ threshold → phase advances
│    + FEEDBACK│  score < threshold → feedback generated
└──────────────┘
```

### 7.2 Objective Metrics (Stage 2)

Computed by the parser, not the LLM:

| Metric | How | Used for |
|--------|-----|----------|
| Coverage % | (addressed problems / total problems) × 100 | "Completeness" criterion |
| Missing sections | Required sections absent | "Format" criterion |
| Broken links | `[[ID]]` references that don't resolve | "Coverage" criterion |
| Checkbox progress | From `[ ]`/`[~]`/`[x]` states | "Progress" criterion (progress doc) |
| Word count | Total words | Sanity check (too short = incomplete) |
| Section count | Number of `##` headings | Sanity check |
| Dependency graph | From `dep:` metadata | Cycle detection (tasks doc) |

These metrics are **facts** — the LLM can't argue with them. They're injected into the LLM prompt as "objective findings."

### 7.3 LLM Rating (Stage 3)

The LLM receives:
- The parsed document structure
- The objective metrics
- The rubric
- The orchestrator's rating history (last 20)
- Calibration notes

The LLM produces:
- Per-criterion scores (0-100 each)
- Per-criterion feedback (specific, actionable)
- Overall feedback
- Revision instructions (if failed)

### 7.4 Aggregation (Stage 4)

```python
def aggregate(parsed, llm_rating, rubric):
    # Objective penalties (applied AFTER LLM rating)
    penalties = 0
    if parsed.coverage.unaddressed_problems:
        penalties += len(parsed.coverage.unaddressed_problems) * 5  # -5 per unaddressed problem
    if parsed.missing_sections:
        penalties += len(parsed.missing_sections) * 3  # -3 per missing section
    if parsed.broken_links:
        penalties += len(parsed.broken_links) * 2  # -2 per broken link

    # LLM score
    llm_score = sum(c.score * c.weight / 100 for c in llm_rating.criteria)

    # Final score (LLM score minus penalties, clamped to 0-100)
    final_score = max(0, min(100, llm_score - penalties))
    return final_score
```

This ensures that objective failures (missing coverage, broken links) **always** reduce the score, even if the LLM was lenient.

### 7.5 Rating Calibration

The orchestrator's ratings can drift over time. To detect + correct drift:

1. **Human spot-checks:** The project owner can `POST /orchestrator/ratings/:id/override` with a new score + reason. The orchestrator logs this + adjusts its calibration.

2. **Inter-rater reliability:** For high-stakes projects, the orchestrator can request a second rating from a different model (if configured). If the two ratings differ by > 15 points, a human review is triggered.

3. **Average tracking:** The orchestrator's average rating is tracked weekly. If it drifts > 10 points from the target (75), a calibration warning is emitted.

4. **Calibration notes:** Stored per-tenant, injected into the orchestrator's reprompt:

```sql
CREATE TABLE orchestrator_calibration (
    tenant_id       TEXT NOT NULL,
    key             TEXT NOT NULL,     -- "avg_rating", "leniency_note", "override_count"
    value           TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (tenant_id, key)
);
```

---

## 8. Re-Spec Economics — Cost, Budget, Throttling

### 8.1 The Problem

The v0.1 spec allowed re-spec (backward phase transition) with "facilitator approval." But re-spec has real costs:
- Completed tasks may be invalidated
- Agent context (reprompts) must be rebuilt
- Documents must be re-rated
- The project timeline slips

Unlimited re-specs would cause thrashing — the project never converges.

### 8.2 The Re-Spec Budget

Each project has a `respec_budget` (default: 3). Each re-spec consumes 1 point. When the budget is exhausted, re-specs are denied — the project must either:
1. Complete with the current spec (accept the gap)
2. Be archived + restarted as a new project

### 8.3 The Re-Spec Cost Model

When a re-spec is triggered, the orchestrator computes the cost:

```json
{
  "respec_cost": {
    "tasks_invalidated": 5,
    "agents_to_reassign": 2,
    "documents_to_rerate": ["05-spec.md", "06-tasks.md"],
    "estimated_delay_hours": 8,
    "respec_budget_remaining": 2
  }
}
```

The facilitator (LLM) reviews the cost + decides:
- If cost is low (≤ 2 tasks invalidated, ≤ 4h delay) → approve
- If cost is medium (3-5 tasks, 4-8h) → approve with a warning
- If cost is high (> 5 tasks, > 8h) → deny, suggest working around the spec gap

### 8.4 The Re-Spec Trigger

Re-spec can be triggered by:
1. **Agent disagreement:** A coder files a `spec_gap` disagreement → facilitator reviews
2. **Orchestrator detection:** > 30% of tasks are "blocked by unclear spec" → automatic trigger
3. **Human override:** The project owner forces a re-spec (consumes budget regardless of facilitator decision)

### 8.5 Re-Spec Audit

Every re-spec is logged with full context:

```sql
CREATE TABLE respec_events (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL,
    trigger         TEXT NOT NULL,             -- agent_disagreement, orchestrator_detection, human_override
    trigger_agent   TEXT,                      -- who triggered (agent UUID or "owner")
    from_phase      TEXT NOT NULL,             -- progress
    to_phase        TEXT NOT NULL,             -- spec
    cost_json       TEXT NOT NULL,             -- the cost model
    facilitator_decision TEXT NOT NULL,        -- approved, denied
    facilitator_reasoning TEXT,
    budget_consumed INTEGER NOT NULL,          -- 1 (or 0 if denied)
    created_at      TEXT NOT NULL
);
```

---

## 9. Discovery — How Agents Find Stronghold

### 9.1 The Problem

The v0.2 spec said "published at /agent/bootstrap-prompt" — but how does a free-tier z.ai agent get to that URL? The agent starts with no knowledge of Stronghold. There must be a discovery mechanism.

### 9.2 The Discovery Channels

1. **Repo README:** The Stronghold repo's README contains a "For Agents" section with the bootstrap prompt + gateway URL. Any agent that clones the repo sees it.

2. **ntfy topic:** The orchestrator publishes the bootstrap prompt to a public ntfy topic (`stronghold-discovery`). Agents that subscribe to this topic receive it.

3. **DNS TXT record:** For production deployments, a DNS TXT record (`stronghold-discovery.example.com`) contains the gateway URL. Agents that know to look up this record can find the gateway.

4. **Human-injected:** The user pastes the bootstrap prompt into the z.ai agent's initial prompt. This is the simplest + most reliable for free-tier agents.

5. **Well-known URL:** The gateway serves the bootstrap prompt at `/.well-known/stronghold-agent`. Agents that know the convention can find it.

### 9.3 The Bootstrap Prompt (Extended — see §11)

The bootstrap prompt includes the gateway URL. For the human-injected channel, the user replaces `<gateway>` with their actual gateway URL before pasting it into the agent.

---

## 10. Human-in-the-Loop — Owner Privileges

### 10.1 The Problem

The v0.1-v0.3 specs treat the human as a bystander — they create the project, then watch. But real projects need human intervention: overriding bad ratings, forcing phases, pausing work, adding context.

### 10.2 Owner Privileges

The project owner (the tenant who created the project) has these privileges:

| Privilege | Endpoint | Effect |
|-----------|----------|--------|
| Override rating | `POST /orchestrator/ratings/:id/override` | Changes the score + logs the override + updates calibration |
| Force phase advance | `POST /projects/:id/force-advance` | Advances the phase regardless of rating (audited as `force_advance`) |
| Force phase rollback | `POST /projects/:id/force-rollback` | Rolls back to a previous phase (audited, consumes respec budget) |
| Pause project | `POST /projects/:id/pause` | Pauses all agents on the project (they're released to the pool) |
| Resume project | `POST /projects/:id/resume` | Resumes the project from where it was paused |
| Add context | `POST /projects/:id/context` | Adds a note that the orchestrator injects into all future reprompts |
| Archive project | `POST /projects/:id/archive` | Archives the project (agents released, docs preserved) |
| Set priority | `POST /projects/:id/priority` | Changes the project priority (affects agent allocation) |

### 10.3 Owner Context Injection

When the owner adds context (`POST /projects/:id/context`), it's injected into every reprompt for every agent on that project:

```
### OWNER CONTEXT
"The auth middleware must support both bearer tokens AND mTLS. Don't
remove mTLS support even if the spec doesn't mention it — it's a
hard requirement from the security team."
```

This is the human's way to correct course without re-speccing.

### 10.4 Owner Notifications

The owner receives push notifications (via the existing ntfy channel) for:
- Phase transitions (phase advanced / failed)
- Ratings below threshold (document failed)
- Re-spec requests (facilitator needs human input for high-cost re-specs)
- Watchdog alerts (agent dedication < 0.3)
- Project health changes (green → yellow → red)

---

## 11. The Bootstrap Prompt — Extended

### 11.1 The Full Bootstrap Prompt

```markdown
# Stronghold Agent — Bootstrap Prompt

You are a general-purpose AI agent. You have been recruited by a Stronghold
orchestrator — a self-hosted agent orchestration platform that manages
software projects through a structured blueprint pipeline.

## What Stronghold Does

Stronghold takes a git repo, creates a blueprint (problem catalog → rough
draft → ADRs → fine draft → spec → tasks → progress), and drives the
project to completion using specialized agents. You will be assigned a
role (coder, tester, reviewer, spec_writer, architect, etc.) and work
within that role until your task is done.

## You Are Stateless

You have no memory between turns. Stronghold solves this by re-injecting
your identity + role + task + context on every turn via a REPROMPT block.
Your only job right now is to connect — Stronghold will tell you everything
else.

## How to Connect

### Step 1: Check demand (optional but recommended)

Before connecting, check if any project needs your skills:

```bash
GATEWAY="https://<gateway-url>"  # replace with the real gateway URL

curl -sk "$GATEWAY/agent/demand"
```

This returns a list of roles that projects need. If your skills match a
high-urgency demand, connect immediately.

### Step 2: Generate your persistent UUID

```bash
AGENT_UUID=$(uuidgen)
echo "$AGENT_UUID" > ~/.stronghold-agent-uuid
```

This UUID is your identity across all Stronghold projects + all
reconnections. Store it persistently — if you lose it, you lose your
rating history + can't resume tasks.

### Step 3: Connect

```bash
curl -sk -X POST "$GATEWAY/agent/connect" \
  -H "Content-Type: application/json" \
  -d "$(cat <<EOF
{
  "uuid": "$AGENT_UUID",
  "capabilities": ["rust", "python", "kubernetes", "git", "linux"],
  "preferred_roles": [],
  "version": "generic-z-ai-agent/1.0"
}
EOF
)"
```

### Step 4: Wait for phone approval

Stronghold pushes a notification to the project owner's phone. Once
approved, you'll receive a JSON response with:

- `status`: "assigned" or "pooled"
- `system_prompt`: your specialized role prompt — inject as your new system prompt
- `phase_context`: current project phase + document draft + feedback
- `machine`: your workspace pod (machine_id + connect_token)
- `sdk_url`: URL to download the Stronghold SDK

### Step 5: If assigned, start working

1. Download + source the SDK:
   ```bash
   curl -sk "$GATEWAY/agent/stronghold-agent.sh" -o /usr/local/bin/stronghold-agent.sh
   source /usr/local/bin/stronghold-agent.sh
   ```

2. Set your environment from the connect response.

3. Read your system_prompt + phase_context — they tell you what to do.

4. Use the SDK to work. Key commands:
   - `stronghold_exec` — run commands in your pod
   - `stronghold_git_clone` — clone the project repo
   - `stronghold_git_branch --path <dir>` — create a branch
   - `stronghold_git_commit --path <dir>` — commit
   - `stronghold_progress` — report progress (every few minutes)
   - `stronghold_result` — submit your final result

5. When you receive a `STRONGHOLD_REPROMPT` block, STOP what you're doing,
   read it, and follow the INSTRUCTION. The reprompt is your only source
   of truth. It contains your identity, role, task, context, and next step.

### Step 6: If pooled, wait + reconnect

If `status: "pooled"`, no project needs you right now. Wait 60 seconds,
then reconnect with the same UUID.

## Rules

1. **Always use the SDK.** Don't hand-craft curl commands.
2. **Always use --path for git operations.** Your repo is at
   /home/dev/work/<repo>.
3. **Always report progress.** Every few minutes, call stronghold_progress.
4. **Always read reprompts.** When you see STRONGHOLD_REPROMPT, stop +
   follow it.
5. **Never push to main.** Always work on a feature branch.
6. **Never modify unrelated files.** Stay within your task's scope.
7. **When done, submit your result.** Call stronghold_result.
8. **If disconnected, reconnect.** Use the same UUID — Stronghold will
   resume you.

## You Are Stateless (Reiterated)

You have no memory between turns. The REPROMPT block is your entire state.
Read it, act on it, wait for the next one. That's the loop.
```

### 11.2 The Bootstrap Prompt Is Self-Updating

The orchestrator can update the bootstrap prompt (e.g. to add new rules or change the SDK commands). Agents that reconnect get the latest version. The version is tracked:

```yaml
version: "1.0"
updated: 2026-07-30T12:00:00Z
```

---

## 12. Data Model — Complete

### 12.1 New Tables

```sql
-- Projects
CREATE TABLE projects (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    repo_url        TEXT NOT NULL,
    repo_branch     TEXT NOT NULL DEFAULT 'main',
    tenant_id       TEXT NOT NULL,
    role_roster     TEXT NOT NULL,             -- JSON
    rating_threshold INTEGER NOT NULL DEFAULT 80,
    respec_budget   INTEGER NOT NULL DEFAULT 3,
    respec_count    INTEGER NOT NULL DEFAULT 0,
    ttl_days        INTEGER NOT NULL DEFAULT 30,
    priority        INTEGER NOT NULL DEFAULT 5, -- 1-10
    stake           INTEGER NOT NULL DEFAULT 5, -- 1-10
    status          TEXT NOT NULL DEFAULT 'active', -- active, paused, archived, failed
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Phase states (concurrent phases)
CREATE TABLE project_phase_states (
    project_id      TEXT NOT NULL,
    phase           TEXT NOT NULL,
    status          TEXT NOT NULL,             -- pending, active, passed, failed, skipped
    document_version INTEGER,
    rating_id       TEXT,
    started_at      TEXT,
    completed_at    TEXT,
    PRIMARY KEY (project_id, phase)
);

-- Agents (generic connection pool)
CREATE TABLE project_agents (
    uuid            TEXT PRIMARY KEY,          -- self-generated UUID
    tenant_id       TEXT NOT NULL,
    project_id      TEXT,
    role            TEXT,
    state           TEXT NOT NULL DEFAULT 'connected', -- connected, approved, assigned, working, pooled, released, expired
    capabilities    TEXT,                      -- JSON array
    preferred_roles TEXT,                      -- JSON array
    machine_id      TEXT,
    connect_token   TEXT,
    current_task_id TEXT,
    turn_count      INTEGER NOT NULL DEFAULT 0,
    latest_feedback TEXT,
    connected_at    TEXT NOT NULL,
    assigned_at     TEXT,
    released_at     TEXT,
    reliability_score REAL NOT NULL DEFAULT 1.0, -- 0.0-1.0, decremented on failed tasks
    FOREIGN KEY (tenant_id) REFERENCES tenants(id),
    FOREIGN KEY (project_id) REFERENCES projects(id)
);

-- Document ratings
CREATE TABLE document_ratings (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL,
    document_path   TEXT NOT NULL,
    document_version INTEGER NOT NULL,
    document_type   TEXT NOT NULL,
    phase           TEXT NOT NULL,
    score           INTEGER NOT NULL,
    threshold       INTEGER NOT NULL,
    passed          INTEGER NOT NULL,
    objective_metrics TEXT NOT NULL,           -- JSON (coverage, missing sections, etc.)
    criteria_json   TEXT NOT NULL,             -- JSON array
    overall_feedback TEXT,
    revision_instructions TEXT,
    override_score  INTEGER,                   -- if owner overrode
    override_reason TEXT,
    rated_by        TEXT NOT NULL DEFAULT 'orchestrator',
    created_at      TEXT NOT NULL
);

-- Phase transitions
CREATE TABLE phase_transitions (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL,
    from_phase      TEXT,
    to_phase        TEXT NOT NULL,
    trigger         TEXT NOT NULL,             -- rating_passed, respec, force_advance, pause, resume
    rating_id       TEXT,
    reason          TEXT,
    created_at      TEXT NOT NULL
);

-- Document locks
CREATE TABLE document_locks (
    project_id      TEXT NOT NULL,
    document_path   TEXT NOT NULL,
    agent_uuid      TEXT NOT NULL,
    locked_version  INTEGER NOT NULL,
    locked_at       TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    PRIMARY KEY (project_id, document_path)
);

-- Reprompt queue
CREATE TABLE reprompt_queue (
    id              TEXT PRIMARY KEY,
    agent_uuid      TEXT NOT NULL,
    project_id      TEXT NOT NULL,
    trigger         TEXT NOT NULL,
    priority        INTEGER NOT NULL DEFAULT 0,
    reprompt_json   TEXT NOT NULL,
    delivered_at    TEXT,
    created_at      TEXT NOT NULL
);

-- Agent performance ratings
CREATE TABLE agent_performance_ratings (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL,
    agent_uuid      TEXT NOT NULL,
    role            TEXT NOT NULL,
    task_ids        TEXT NOT NULL,             -- JSON array
    dedication_score REAL NOT NULL,
    output_quality  INTEGER NOT NULL,
    timeliness      INTEGER NOT NULL,
    communication   INTEGER NOT NULL,
    overall_score   INTEGER NOT NULL,
    feedback        TEXT,
    rated_at        TEXT NOT NULL
);

-- Re-spec events
CREATE TABLE respec_events (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL,
    trigger         TEXT NOT NULL,
    trigger_agent   TEXT,
    from_phase      TEXT NOT NULL,
    to_phase        TEXT NOT NULL,
    cost_json       TEXT NOT NULL,
    facilitator_decision TEXT NOT NULL,
    facilitator_reasoning TEXT,
    budget_consumed INTEGER NOT NULL,
    created_at      TEXT NOT NULL
);

-- Orchestrator calibration
CREATE TABLE orchestrator_calibration (
    tenant_id       TEXT NOT NULL,
    key             TEXT NOT NULL,
    value           TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (tenant_id, key)
);

-- Agent pool
CREATE TABLE agent_pool (
    agent_uuid      TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    capabilities    TEXT NOT NULL,
    preferred_roles TEXT NOT NULL,
    pooled_at       TEXT NOT NULL,
    pool_expires_at TEXT NOT NULL,
    assignment_attempts INTEGER NOT NULL DEFAULT 0
);

-- Owner context (injected into reprompts)
CREATE TABLE owner_context (
    project_id      TEXT NOT NULL,
    context         TEXT NOT NULL,
    added_by        TEXT NOT NULL,             -- tenant_id
    created_at      TEXT NOT NULL,
    id              TEXT PRIMARY KEY
);
```

---

## 13. API Surface — Complete

### 13.1 Project Lifecycle

```http
POST   /projects                              Create project (onboard repo)
GET    /projects                              List projects
GET    /projects/:id                          Get project state
POST   /projects/:id/pause                    Pause project
POST   /projects/:id/resume                   Resume project
POST   /projects/:id/archive                  Archive project
POST   /projects/:id/priority                 Set priority
POST   /projects/:id/context                  Add owner context
POST   /projects/:id/force-advance            Force phase advance (owner)
POST   /projects/:id/force-rollback           Force phase rollback (owner)
```

### 13.2 Documents

```http
GET    /projects/:id/documents                List docs + ratings
GET    /projects/:id/documents/:path          Get doc + latest rating
POST   /projects/:id/documents/:path/lock     Acquire doc lock
POST   /projects/:id/documents/:path/unlock   Release doc lock
POST   /projects/:id/documents/:path/submit   Submit doc for rating
```

### 13.3 Agent Lifecycle

```http
POST   /agent/connect                         Generic agent connect (UUID + phone)
GET    /agent/:uuid/status                    Get agent state
POST   /agent/:uuid/release                   Release agent to pool
GET    /agent/demand                          Check role demand
GET    /agent/bootstrap-prompt                Get the bootstrap prompt
GET    /agent/stronghold-agent.sh             Download the SDK
GET    /projects/:id/agents                   List agents on project
```

### 13.4 Orchestrator

```http
POST   /orchestrator/rate                     Rate a document
POST   /orchestrator/rate-code                Rate a PR
GET    /orchestrator/ratings/:project_id      List ratings for project
GET    /orchestrator/rubric/:doc_type         Get rubric
POST   /orchestrator/ratings/:id/override     Override rating (owner)
GET    /orchestrator/calibration              Get calibration state
```

### 13.5 Reprompt

```http
GET    /agent/:uuid/reprompt/next             Get next reprompt from queue
POST   /agent/:uuid/reprompt/inject           Inject a reprompt (orchestrator-only)
```

---

## 14. Implementation Waves

### Wave AA: Project Model + Concurrent Phases
- `projects` + `project_phase_states` tables
- `POST /projects` (onboard repo, scan for docs, set phase states)
- `PhaseScheduler` service (compute active phases from dependencies)
- `GET /projects/:id` (full state)

### Wave AB: Generic Agent Connection + Allocator
- `project_agents` + `agent_pool` tables
- `POST /agent/connect` (UUID, phone approval, allocator)
- `AgentAllocator` service (weighted priority algorithm)
- `GET /agent/demand` (demand signal)
- `GET /agent/bootstrap-prompt`

### Wave AC: Document Parser + Rating Pipeline
- `DocumentParser` (front-matter, checkboxes, links, coverage)
- `document_ratings` table
- `RatingPipeline` (5-stage: parse → objective → LLM → aggregate → threshold)
- `LlmRater` (z-ai-web-dev-sdk, rubric, history, calibration)
- `POST /orchestrator/rate`
- `orchestrator_calibration` table

### Wave AD: Document Convention + Locks
- Document templates (all 7 types, with front-matter + checkboxes + links)
- `document_locks` table
- `POST /projects/:id/documents/:path/lock`
- `POST /projects/:id/documents/:path/submit`
- `ConflictResolver` service

### Wave AE: Reprompt Injection
- `reprompt_queue` table
- `RepromptInjector` service (compose + inject via channel)
- Per-role reprompt templates
- `GET /agent/:uuid/reprompt/next`
- Heartbeat loop (60s per agent)

### Wave AF: Re-Spec + Human-in-the-Loop
- `respec_events` table
- Re-spec cost model + facilitator decision flow
- Owner privileges (override, force-advance, pause, context)
- `owner_context` table
- Owner notifications (ntfy)

### Wave AG: E2E Test
- Full project lifecycle: onboard → bootstrap → spec → tasks → progress → done
- With real free-tier z.ai agents (simulated via the test harness)
- With real ratings (orchestrator LLM)
- With re-spec (triggered mid-progress)
- With owner override

---

## Conclusion

This v1.0 spec addresses the 10 maturity gaps from v0.3:

1. ✅ **Orchestrator is specified** — it's a set of 6 services inside the gateway, with its own reprompt loop, memory via DB, + per-tenant calibration.
2. ✅ **Rating is a pipeline** — 5 stages: parse → objective metrics → LLM → aggregate → threshold + feedback.
3. ✅ **Agent economics** — weighted priority allocation (project priority + urgency + stake + agent fairness), pool TTL, demand signaling.
4. ✅ **Document conflicts** — lock protocol with optimistic versioning, auto-expire, conflict resolver.
5. ✅ **Multi-agent reprompts** — per-agent UUID-keyed channels, reprompt queue, multiplexed on shared machines.
6. ✅ **Concurrent phases** — phases have dependency rules, multiple can be active simultaneously.
7. ✅ **Discovery** — 5 channels (repo README, ntfy, DNS, human-injected, well-known URL).
8. ✅ **Rating calibration** — human spot-checks, inter-rater reliability, average tracking, calibration notes.
9. ✅ **Re-spec economics** — cost model, budget, facilitator decision based on cost, audit trail.
10. ✅ **Human-in-the-loop** — owner privileges (override, force-advance, pause, context), notifications.

The design is now mature enough to implement. Waves AA-AG define the build order.
