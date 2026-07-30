# Blueprint Orchestrator Specification

> **Status:** Design — v0.1
> **Authors:** Stronghold
> **Created:** 2026-07-30
> **Replaces:** Ad-hoc task/workflow model
>
> **Vision:** You hand the orchestrator a REPO. The orchestrator "logs into"
> it — clones, scans, and determines whether the repo has the mandatory
> blueprint documents. If not, it enters a BOOTSTRAP phase where
> specialized agents create them. Once the blueprint is complete, the
> orchestrator decomposes the spec into tasks, assigns roles to generic
> agents that connect (phone-approved), and drives the project to
> completion through a state machine. Every phase transition is gated by
> the orchestrator rating the phase's document(s) against a rubric.

---

## 1. The Mental Model

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         THE ORCHESTRATOR                                │
│                                                                         │
│  ┌──────────┐   rates   ┌──────────────┐   advances   ┌──────────────┐  │
│  │  REPO    │──────────►│  BLUEPRINT   │─────────────►│  EXECUTION   │  │
│  │ (cloned) │           │  (7 docs)    │              │  (tasks)     │  │
│  └──────────┘           └──────────────┘              └──────────────┘  │
│       ▲                      ▲                               ▲          │
│       │                      │                               │          │
│    onboard              generic agents                  generic agents   │
│                         (spec_writer role)              (coder, etc.)    │
│                                                                         │
│  Every transition gated by orchestrator document rating (0-100)         │
└─────────────────────────────────────────────────────────────────────────┘
```

**Key principles:**

1. **REPO-first.** The unit of work is a project = a git repo + a blueprint state machine. Not a task. Not a workflow. A project.

2. **Blueprint-mandatory.** Every project MUST have 7 documents in a strict pipeline. No document → no next phase. No exceptions. If docs are missing, the orchestrator enters BOOTSTRAP and creates them before any implementation work begins.

3. **Generic-agent-connects.** Agents don't get pre-assigned to projects. An agent connects generically (self-generated UUID, phone-approved), then the orchestrator inspects all active projects, finds one that needs the agent's capabilities, assigns a role, and sends the agent its system prompt + phase context.

4. **State-machine-driven.** The project moves through phases. Each phase has entry criteria, a document it produces, and exit criteria (orchestrator rating ≥ threshold). Transitions are explicit, logged, and reversible only via a controlled re-spec.

5. **Orchestrator-rates-documents.** The orchestrator is itself an agent. Its job is to read documents, apply rubrics, produce scores + structured feedback, and decide whether a phase can advance. It does NOT write code. It judges.

---

## 2. The Blueprint — 7 Documents

Every project MUST produce these 7 documents, in this order. Each lives at a fixed path in the repo so the orchestrator can find them:

| # | Document | Path | Phase | Produced by |
|---|----------|------|-------|-------------|
| 1 | Problem Catalog | `docs/blueprint/01-problem-catalog.md` | `problem_catalog` | spec_writer |
| 2 | Rough Draft | `docs/blueprint/02-rough-draft.md` | `rough_draft` | spec_writer |
| 3 | ADRs | `docs/blueprint/03-adrs/` (directory, one .md per ADR) | `adrs` | architect |
| 4 | Fine Draft | `docs/blueprint/04-fine-draft.md` | `fine_draft` | architect |
| 5 | Spec | `docs/blueprint/05-spec.md` | `spec` | architect |
| 6 | Tasks | `docs/blueprint/06-tasks.md` | `tasks` | planner |
| 7 | Progress | `docs/blueprint/07-progress.md` | `progress` | integrator (living) |

### 2.1 Document Rubrics

Each document type has a rubric — a set of weighted criteria that the orchestrator scores 0-100. A phase advances when its document scores ≥ the project's configured threshold (default 80).

#### Problem Catalog Rubric (100 pts)
| Criterion | Weight | Question |
|-----------|--------|----------|
| Completeness | 25 | Are all problems listed? No obvious gaps? |
| Clarity | 25 | Is each problem stated unambiguously? |
| Prioritization | 20 | Are problems ranked (must-have, should-have, nice-to-have)? |
| Constraints | 15 | Are constraints (time, budget, regulatory, technical) identified? |
| Stakeholders | 15 | Are stakeholders + their needs identified? |

#### Rough Draft Rubric (100 pts)
| Criterion | Weight | Question |
|-----------|--------|----------|
| Coverage | 30 | Addresses every problem from the catalog? |
| Feasibility | 25 | Are proposed solutions technically feasible? |
| Alternatives | 20 | Were alternatives considered + rejected with reasons? |
| Risks | 15 | Are risks identified + mitigations sketched? |
| Clarity | 10 | Is the writing clear + concise? |

#### ADRs Rubric (100 pts)
| Criterion | Weight | Question |
|-----------|--------|----------|
| Coverage | 25 | One ADR per major architectural decision? |
| Context | 20 | Is the context (forces, constraints) documented? |
| Decision | 20 | Is the decision clearly stated? |
| Consequences | 20 | Are positive + negative consequences documented? |
| Format | 15 | Follows the ADR template (NYU/madr format)? |

#### Fine Draft Rubric (100 pts)
| Criterion | Weight | Question |
|-----------|--------|----------|
| Architecture | 25 | Is the architecture clearly described (diagrams + prose)? |
| Interfaces | 20 | Are module/component interfaces defined? |
| Data model | 20 | Is the data model documented (ERD or equivalent)? |
| Security | 15 | Are security considerations addressed? |
| Testability | 10 | Is the design testable (test strategy sketched)? |
| Consistency | 10 | Consistent with the ADRs? |

#### Spec Rubric (100 pts)
| Criterion | Weight | Question |
|-----------|--------|----------|
| Requirements | 25 | Are functional + non-functional requirements listed? |
| Acceptance criteria | 25 | Does each requirement have testable acceptance criteria? |
| Edge cases | 20 | Are edge cases + error scenarios addressed? |
| Dependencies | 15 | Are external dependencies (libraries, services, APIs) listed? |
| Format | 15 | Follows the spec template? |

#### Tasks Rubric (100 pts)
| Criterion | Weight | Question |
|-----------|--------|----------|
| Coverage | 25 | Do tasks cover all spec requirements? |
| Granularity | 20 | Are tasks appropriately sized (1-4 hours each)? |
| Dependencies | 20 | Are task dependencies mapped (DAG)? |
| Estimation | 15 | Are effort estimates provided? |
| Assignability | 10 | Can each task be assigned to a single role? |
| Definition of Done | 10 | Does each task have a clear DoD? |

#### Progress Rubric (100 pts, rated continuously — not a gate)
| Criterion | Weight | Question |
|-----------|--------|----------|
| Current status | 20 | Is the current state clear? |
| Blockers | 20 | Are blockers identified + escalated? |
| Next steps | 20 | Are next steps clear? |
| Velocity | 20 | Is progress measurable (burndown or equivalent)? |
| Emerging risks | 20 | Are new risks documented? |

---

## 3. The Project State Machine

```
                         ┌──────────────────────────────────────────┐
                         │                                          │
                         ▼                                          │
                   ┌──────────┐                                     │
   onboard repo ──►│  INIT    │ clone repo, scan for blueprint docs  │
                   └────┬─────┘                                     │
                        │                                           │
            ┌───────────┴───────────┐                               │
            │ docs missing          │ docs present                  │
            ▼                       ▼                               │
     ┌──────────────┐        ┌──────────────┐                       │
     │  BOOTSTRAP   │        │   RESUME     │ determine phase       │
     └──────┬───────┘        └──────┬───────┘                       │
            │                       │                               │
            ▼                       ▼                               │
     ┌──────────────────┐    ┌──────────────────┐                   │
     │ PROBLEM_CATALOG  │    │  <current phase> │                   │
     └──────┬───────────┘    └──────┬───────────┘                   │
            │ rating ≥ 80           │                               │
            ▼                       │                               │
     ┌──────────────────┐           │                               │
     │  ROUGH_DRAFT     │           │                               │
     └──────┬───────────┘           │                               │
            │ rating ≥ 80           │                               │
            ▼                       │                               │
     ┌──────────────────┐           │                               │
     │      ADRS        │           │                               │
     └──────┬───────────┘           │                               │
            │ rating ≥ 80           │                               │
            ▼                       │                               │
     ┌──────────────────┐           │                               │
     │   FINE_DRAFT     │           │                               │
     └──────┬───────────┘           │                               │
            │ rating ≥ 80           │                               │
            ▼                       │                               │
     ┌──────────────────┐           │                               │
     │      SPEC        │           │                               │
     └──────┬───────────┘           │                               │
            │ rating ≥ 80           │                               │
            ▼                       │                               │
     ┌──────────────────┐           │                               │
     │      TASKS       │           │                               │
     └──────┬───────────┘           │                               │
            │ rating ≥ 80           │                               │
            ▼                       │                               │
     ┌──────────────────┐           │                               │
     │    PROGRESS      │◄──────────┘                               │
     └──────┬───────────┘                                           │
            │ all tasks done                                        │
            ▼                                                       │
     ┌──────────┐                                                   │
     │   DONE   │                                                   │
     └──────────┘                                                   │
            ▲                                                       │
            │ re-spec triggered (task reveals spec gap)             │
            └───────────────────────────────────────────────────────┘
```

### 3.1 Phase Definitions

| Phase | Entry Criteria | Document Produced | Exit Criteria | Roles Active |
|-------|---------------|-------------------|---------------|--------------|
| `INIT` | repo onboarded | — | scan complete | orchestrator |
| `BOOTSTRAP` | docs missing | — | docs scan done | orchestrator |
| `PROBLEM_CATALOG` | BOOTSTRAP complete | `01-problem-catalog.md` | rating ≥ threshold | spec_writer |
| `ROUGH_DRAFT` | problem_catalog rated ≥ threshold | `02-rough-draft.md` | rating ≥ threshold | spec_writer |
| `ADRS` | rough_draft rated ≥ threshold | `03-adrs/*.md` | rating ≥ threshold | architect |
| `FINE_DRAFT` | adrs rated ≥ threshold | `04-fine-draft.md` | rating ≥ threshold | architect |
| `SPEC` | fine_draft rated ≥ threshold | `05-spec.md` | rating ≥ threshold | architect |
| `TASKS` | spec rated ≥ threshold | `06-tasks.md` | rating ≥ threshold | planner |
| `PROGRESS` | tasks rated ≥ threshold | `07-progress.md` (living) | all tasks done | coder, tester, reviewer, integrator, watchdog |
| `DONE` | all tasks complete | — | — | — |

### 3.2 Backward Transitions (Re-Spec)

Sometimes a task reveals a spec gap. The project can transition backward:

```
PROGRESS ──(re-spec triggered)──► SPEC ──(revised)──► TASKS ──(revised)──► PROGRESS
```

Re-spec is triggered by:
- A coder agent filing a `spec_gap` disagreement (facilitator approves)
- The orchestrator detecting > 30% of tasks are "blocked by unclear spec"
- A reviewer flagging that an implementation doesn't match the spec (and the spec is wrong, not the implementation)

Re-spec is NOT automatic — the facilitator role must approve it, and the orchestrator records a `respec_event` in the audit log with the reason.

### 3.3 Project Configuration

When a user creates a project, they provide:

```json
{
  "repo_url": "https://github.com/acme/widget.git",
  "repo_branch": "main",
  "name": "Widget v2",
  "role_roster": {
    "spec_writer": 1,
    "architect": 1,
    "planner": 1,
    "coder": 3,
    "tester": 1,
    "reviewer": 1,
    "integrator": 1,
    "watchdog": 1,
    "oracle": 1,
    "facilitator": 1
  },
  "rating_threshold": 80,
  "bootstrap": true,
  "max_respecs": 3,
  "ttl_days": 30
}
```

- `role_roster`: how many of each role the project needs. The orchestrator uses this to decide which generic agents to assign.
- `rating_threshold`: the minimum document rating to advance (default 80).
- `bootstrap`: if true, missing docs are created by bootstrap agents. If false, the project is rejected until docs exist.
- `max_respecs`: how many backward transitions are allowed before the project is flagged as "thrashing."
- `ttl_days`: the project auto-archives after this many days of inactivity.

---

## 4. The Agent Lifecycle State Machine

```
┌─────────┐     ┌──────────┐     ┌──────────┐     ┌──────────┐     ┌─────────┐
│ CONNECT │────►│ APPROVED │────►│ ASSIGNED │────►│ WORKING  │────►│RELEASED │
└─────────┘     └──────────┘     └──────────┘     └──────────┘     └─────────┘
     │               │                 │                 │
     │ phone deny    │ timeout         │ reassign        │ task done
     ▼               ▼                 ▼                 ▼
┌─────────┐    ┌──────────┐    ┌──────────┐     ┌──────────┐
│ DENIED  │    │ EXPIRED  │    │ POOLED   │     │ REPORTED │
└─────────┘    └──────────┘    └──────────┘     └──────────┘
                                    │
                                    │ new project needs this role
                                    ▼
                              ┌──────────┐
                              │ ASSIGNED │
                              └──────────┘
```

### 4.1 The Generic Agent Connection Protocol

An agent does NOT know which project it will work on when it connects. The flow:

```
1. Agent generates a UUID (client-side, stored locally)
   uuid = uuid4()

2. Agent POSTs /agent/connect
   {
     "uuid": "<uuid>",
     "capabilities": ["rust", "k8s", "postgres"],   // optional
     "preferred_roles": ["coder", "tester"],         // optional
     "version": "stronghold-agent/1.0"
   }

3. Orchestrator:
   a. Creates a pending_agent record with the UUID
   b. Pushes phone approval request (WebAuthn) — the notification
      includes the UUID (truncated) so the human can verify
   c. Long-polls for phone decision (60s timeout)

4. Phone approves (WebAuthn assertion verified)

5. Orchestrator:
   a. Marks agent as APPROVED
   b. Inspects ALL active projects — finds projects whose current
      phase has unmet role demand matching the agent's capabilities
   c. If a match found:
      - Assigns the agent a role from the project's roster
      - Returns {
          project_id, role, system_prompt,
          current_phase, phase_context,
          machine_id, connect_token
        }
   d. If no match found:
      - Returns { status: "pooled", message: "No project needs your
        capabilities right now. You are in the agent pool." }
      - Agent enters POOLED state, gets notified when a project needs it

6. Agent begins working in the assigned phase
   - Receives the phase's system prompt (role-specific)
   - Receives the phase context (the current document draft, rubric, feedback)
   - Works via stronghold_exec / stronghold_git_* / message bus

7. Agent reports progress + result
   - stronghold_progress (periodic)
   - stronghold_result (final)
   - Orchestrator rates the document
   - If rating ≥ threshold → phase advances, agent may be reassigned
   - If rating < threshold → feedback sent to agent, agent revises
```

### 4.2 Role Assignment Logic

The orchestrator's role assignment algorithm:

```python
def assign_role(agent):
    for project in active_projects_sorted_by_priority():
        phase = project.current_phase
        demand = phase_role_demand(phase)  # see table below
        for role, count_needed in demand.items():
            count_assigned = count_agents_with_role(project.id, role)
            if count_assigned < count_needed:
                if agent.capabilities matches role_requirements(role):
                    if agent.preferred_roles is empty or role in agent.preferred_roles:
                        return Assign(project, role)
    return Pool(agent)
```

**Phase → Role Demand mapping:**

| Phase | Roles needed (count) |
|-------|---------------------|
| `BOOTSTRAP` | spec_writer (1) |
| `PROBLEM_CATALOG` | spec_writer (1) |
| `ROUGH_DRAFT` | spec_writer (1) |
| `ADRS` | architect (1) |
| `FINE_DRAFT` | architect (1) |
| `SPEC` | architect (1) |
| `TASKS` | planner (1) |
| `PROGRESS` | coder (roster), tester (1), reviewer (1), integrator (1), watchdog (1), oracle (1), facilitator (1) |
| `DONE` | — |

The orchestrator respects the project's `role_roster` — if the roster says 3 coders, the orchestrator accepts up to 3 generic agents into coder roles during the PROGRESS phase.

### 4.3 The Bootstrap Role: `spec_writer`

A new role not in the original 9: the **spec_writer**. This agent specializes in creating the blueprint documents (problem catalog, rough draft). It does NOT write code. It writes prose.

System prompt summary:
> You are a Spec Writer. You create the problem catalog and rough draft.
> You read the repo, understand the domain, interview stakeholders (via
> the facilitator), and produce clear, complete, structured documents.
> You do NOT write code. You do NOT create branches. You write markdown.

---

## 5. The Orchestrator Rating System

The orchestrator is itself an agent. Its core function: **read a document, apply a rubric, produce a score + structured feedback.**

### 5.1 Rating Request

```http
POST /orchestrator/rate
{
  "project_id": "<project>",
  "document_path": "docs/blueprint/01-problem-catalog.md",
  "document_type": "problem_catalog",
  "phase": "problem_catalog"
}
```

### 5.2 Rating Response

```json
{
  "rating_id": "rat_01KY...",
  "document_type": "problem_catalog",
  "score": 72,
  "threshold": 80,
  "passed": false,
  "criteria": [
    {
      "criterion": "Completeness",
      "weight": 25,
      "score": 18,
      "feedback": "Missing the latency requirement. The catalog mentions throughput but not p99 latency targets."
    },
    {
      "criterion": "Clarity",
      "weight": 25,
      "score": 22,
      "feedback": "Problem 3 is ambiguous — 'the system should be fast' is not measurable."
    },
    ...
  ],
  "overall_feedback": "The catalog is a solid start but needs measurable acceptance criteria for each problem. Revise problems 3, 7, and 12 to include specific numbers (latency, throughput, error rate).",
  "revision_instructions": "Address the 3 issues above. Resubmit within 1 hour."
}
```

### 5.3 Rating Storage

Every rating is stored in the `document_ratings` table:

```sql
CREATE TABLE document_ratings (
    id              TEXT PRIMARY KEY,         -- rat_<ULID>
    project_id      TEXT NOT NULL,
    document_path   TEXT NOT NULL,
    document_type   TEXT NOT NULL,            -- problem_catalog, rough_draft, etc.
    phase           TEXT NOT NULL,
    score           INTEGER NOT NULL,         -- 0-100
    threshold       INTEGER NOT NULL,         -- project's configured threshold
    passed          INTEGER NOT NULL,         -- 0 or 1
    criteria_json   TEXT NOT NULL,            -- JSON array of {criterion, weight, score, feedback}
    overall_feedback TEXT,
    revision_instructions TEXT,
    rated_by        TEXT NOT NULL,            -- "orchestrator"
    created_at      TEXT NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id)
);
```

### 5.4 Rating-Driven Transitions

When a rating is produced:

```python
def on_rating(rating):
    if rating.passed:
        # Advance to next phase
        project.phase = next_phase(project.phase)
        audit_log("phase_advanced", {from, to, rating_id})
        notify_role_demand_changed(project)
    else:
        # Send feedback to the role agent that produced the doc
        agent = find_agent_for_document(rating.project_id, rating.document_path)
        send_message(agent, "revision_requested", {
            rating_id: rating.id,
            feedback: rating.overall_feedback,
            instructions: rating.revision_instructions
        })
        # Agent revises + resubmits → orchestrator re-rates
```

### 5.5 Rating Concurrency

The orchestrator rates one document at a time per project (to avoid conflicting feedback). Ratings are idempotent — re-rating the same document version produces the same score (the orchestrator is deterministic given the same document + rubric + model version).

---

## 6. API Surface (new endpoints)

### 6.1 Project Lifecycle

```http
POST   /projects                          Create a project (onboard a repo)
GET    /projects                          List projects
GET    /projects/:id                      Get project state (phase, ratings, agents)
POST   /projects/:id/advance              Force phase advance (orchestrator-only, normally auto)
POST   /projects/:id/respec               Trigger a re-spec (facilitator-only)
GET    /projects/:id/documents            List blueprint documents + their ratings
GET    /projects/:id/documents/:path      Get a document + its latest rating
POST   /projects/:id/documents/:path/submit  Submit a document for rating
```

### 6.2 Agent Lifecycle

```http
POST   /agent/connect                     Generic agent connects (UUID + phone approval)
GET    /agent/:uuid/status                Get agent state (pooled, assigned, working, released)
POST   /agent/:uuid/release               Release an agent back to the pool
GET    /projects/:id/agents               List agents assigned to a project
```

### 6.3 Orchestrator Rating

```http
POST   /orchestrator/rate                 Rate a document (orchestrator-only)
GET    /orchestrator/ratings/:project_id  List all ratings for a project
GET    /orchestrator/rubric/:doc_type     Get the rubric for a document type
```

### 6.4 Phase Context

```http
GET    /projects/:id/phase-context        Get the current phase context
       (returns: phase, system_prompt for the active role, current document
        draft, rubric, latest feedback, task list if in PROGRESS)
```

---

## 7. Data Model (new tables)

### 7.1 Projects

```sql
CREATE TABLE projects (
    id              TEXT PRIMARY KEY,          -- proj_<ULID>
    name            TEXT NOT NULL,
    repo_url        TEXT NOT NULL,
    repo_branch     TEXT NOT NULL DEFAULT 'main',
    tenant_id       TEXT NOT NULL,
    phase           TEXT NOT NULL DEFAULT 'init',  -- init, bootstrap, problem_catalog, ...
    role_roster     TEXT NOT NULL,             -- JSON: {"coder": 3, "architect": 1, ...}
    rating_threshold INTEGER NOT NULL DEFAULT 80,
    max_respecs     INTEGER NOT NULL DEFAULT 3,
    respec_count    INTEGER NOT NULL DEFAULT 0,
    ttl_days        INTEGER NOT NULL DEFAULT 30,
    status          TEXT NOT NULL DEFAULT 'active',  -- active, archived, failed
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);
```

### 7.2 Agents (generic connection pool)

```sql
CREATE TABLE project_agents (
    id              TEXT PRIMARY KEY,          -- agent UUID (self-generated)
    tenant_id       TEXT NOT NULL,
    project_id      TEXT,                      -- NULL when pooled
    role            TEXT,                      -- NULL when pooled
    state           TEXT NOT NULL DEFAULT 'connected',  -- connected, approved, assigned, working, pooled, released
    capabilities    TEXT,                      -- JSON array
    preferred_roles TEXT,                      -- JSON array
    machine_id      TEXT,                      -- k8s pod (when working)
    connect_token   TEXT,                      -- pod connect token (when working)
    current_task_id TEXT,                      -- task being worked on (when working)
    connected_at    TEXT NOT NULL,
    assigned_at     TEXT,
    released_at     TEXT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id),
    FOREIGN KEY (project_id) REFERENCES projects(id)
);
```

### 7.3 Document Ratings

(see §5.3 above)

### 7.4 Phase Transitions (audit)

```sql
CREATE TABLE phase_transitions (
    id              TEXT PRIMARY KEY,          -- pt_<ULID>
    project_id      TEXT NOT NULL,
    from_phase      TEXT NOT NULL,
    to_phase        TEXT NOT NULL,
    trigger         TEXT NOT NULL,             -- rating_passed, respec, manual
    rating_id       TEXT,                      -- FK to document_ratings (when trigger=rating_passed)
    reason          TEXT,
    created_at      TEXT NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id),
    FOREIGN KEY (rating_id) REFERENCES document_ratings(id)
);
```

---

## 8. The Orchestrator as an Agent

The orchestrator is NOT a human. It is a special agent that:

1. **Runs inside the gateway** (not in a pod — it's a built-in service).
2. **Has its own system prompt** focused on rating documents, not writing code.
3. **Uses the LLM** (via the z-ai-web-dev-sdk or a configured model) to read documents + apply rubrics.
4. **Is deterministic + auditable** — every rating is logged with the full prompt + response.
5. **Cannot be bypassed** — phase transitions are ONLY triggered by the orchestrator's rating endpoint. No manual "advance" unless the project is in a stuck state (and even then, it requires a `force_advance` audit event).

### 8.1 Orchestrator System Prompt (excerpt)

```
You are the Stronghold Orchestrator. Your job: rate blueprint documents.

You receive:
- A document (markdown)
- A document type (problem_catalog, rough_draft, adrs, fine_draft, spec, tasks, progress)
- A rubric (criteria + weights)

You produce:
- A score (0-100)
- Per-criterion scores + feedback
- Overall feedback
- Revision instructions (if score < threshold)

Rules:
- Be strict but fair. A score of 80 means "good enough to proceed."
- Be specific. "This is unclear" is useless. "Problem 3 says 'fast' but
  doesn't define what fast means — add a specific latency target" is useful.
- Check for dependencies. The spec must address every problem in the
  catalog. The tasks must cover every requirement in the spec.
- Flag missing sections explicitly. If the spec has no "Error handling"
  section, say so.
- Do NOT write the document. Do NOT suggest specific wording. Only
  describe what's missing or unclear.
```

---

## 9. End-to-End Flow Example

```
1. User: POST /projects
   { "repo_url": "https://github.com/acme/widget.git", "name": "Widget v2",
     "role_roster": {"spec_writer": 1, "architect": 1, "coder": 2, ...} }

2. Orchestrator: clones repo, scans for docs/blueprint/
   → no docs found → phase = BOOTSTRAP

3. Orchestrator: generates a generic agent prompt, publishes it
   (the prompt tells agents how to connect: POST /agent/connect with a UUID)

4. Generic Agent A: generates UUID, POSTs /agent/connect
   → phone approval → APPROVED
   → orchestrator inspects projects → Widget v2 is in BOOTSTRAP, needs spec_writer
   → assigns Agent A the spec_writer role
   → returns { project_id, role: "spec_writer", phase: "problem_catalog",
              system_prompt: "...", phase_context: { repo_path, rubric } }

5. Agent A (spec_writer): reads the repo, writes docs/blueprint/01-problem-catalog.md
   → POST /projects/:id/documents/01-problem-catalog.md/submit

6. Orchestrator: rates the document against the problem_catalog rubric
   → score 72 < 80 → sends feedback to Agent A
   → Agent A revises → resubmits
   → score 85 ≥ 80 → phase advances to ROUGH_DRAFT
   → audit: phase_transitions (problem_catalog → rough_draft, rating_passed)

7. Agent A (spec_writer): writes 02-rough-draft.md → rated → advances
   ... (repeat for ADRs, fine_draft, spec, tasks)

8. PROGRESS phase: orchestrator needs coders
   → Generic Agents B, C, D connect → assigned coder roles
   → Agent E connects → assigned tester role
   → Agent F connects → assigned reviewer role
   → etc.

9. Coders work on tasks, testers test, reviewers review, integrator merges
   → Watchdog monitors dedication
   → Progress document updated continuously
   → Orchestrator rates progress document weekly (not a gate, just feedback)

10. All tasks done → phase = DONE → project archived
```

---

## 10. Implementation Plan (Waves AA-AF)

This spec is implemented in 6 waves following the v1.2.0 hardening prompt's structure:

| Wave | Theme | Tasks |
|------|-------|-------|
| **AA** | Project model + state machine | projects table, phase enum, /projects endpoints, repo cloning |
| **AB** | Generic agent connection | /agent/connect, project_agents table, UUID-based phone approval, role assignment logic |
| **BC** | Document rating system | document_ratings table, /orchestrator/rate, rubric definitions, LLM-based rating |
| **AD** | Blueprint documents + bootstrap | spec_writer role, BOOTSTRAP phase, document submission endpoints |
| **AE** | Phase transitions + re-spec | transition logic, backward transitions, facilitator approval for re-spec |
| **AF** | E2E test + integration | full project lifecycle test: onboard → bootstrap → spec → tasks → progress → done |

Each wave follows the same DoD pattern: code + tests + commit + push + verify.

---

## 11. Open Questions

1. **LLM for rating:** Which model does the orchestrator use? GLM-5.2 (via z-ai-web-dev-sdk)? A local model? Configurable per-tenant?
2. **Rating cost:** Each rating is an LLM call. For a 7-document pipeline with revisions, that's 15-30 calls. Should we cache? Rate-limit?
3. **Human override:** Can a human force-advance a phase if the orchestrator is wrong? (Yes, but it's audited as `force_advance`.)
4. **Multi-repo projects:** Can a project span multiple repos (monorepo + infra repo)? (Future: yes, via subprojects.)
5. **Agent reconnection:** If an agent's pod dies mid-task, can it reconnect + resume? (Yes, via the UUID — the orchestrator tracks the agent's state in the DB, not in-memory.)
6. **Document conflicts:** If two spec_writers are assigned (roster says 2), how do they coordinate? (Via the message bus + a "document lock" — only one agent edits at a time, the other reviews.)

---

## 12. Relationship to Existing Stronghold

This spec does NOT replace the existing task/workflow model — it **layers on top of it**:

- **Projects** contain **tasks** (the existing model). The PROGRESS phase is just a collection of tasks.
- **Workflows** (DAG engine) are used WITHIN the PROGRESS phase for multi-step task execution.
- **Roles** (the existing 9 + the new spec_writer) are assigned to agents per-project.
- **Audit log** records phase transitions, ratings, role assignments — all dual-signed.
- **Watchdog** monitors agents during PROGRESS (same as today).
- **WebAuthn** phone approval is used for agent connection (same as session approval, but keyed to the agent UUID).

The existing `/agent/order` + `/agent/exec` + `/agent/task` endpoints remain — they're used by agents that have been assigned to a project. The new `/agent/connect` endpoint is the entry point for generic agents.

---

## Conclusion

This spec transforms Stronghold from a "task executor with a nice API" into a **project orchestrator with a document-driven state machine**. The key insight: the orchestrator's job is not to write code — it's to **judge documents** and **drive a project through phases**. Agents connect generically, get specialized by the orchestrator, and work within the current phase. Every transition is gated by a rating. Every rating is audited. Every document has a rubric.

This is the blueprint.
