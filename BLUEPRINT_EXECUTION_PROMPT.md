# BLUEPRINT ORCHESTRATOR EXECUTION PROMPT — Waves BA through BG

> **Orchestrator contract:** You are the orchestrator. You must not return
> until every wave's Definition of Done (DoD) passes. Each wave is decomposed
> into granular tasks (≤500 lines each) assigned to sub-agents. Each task
> produces exactly one commit. Each wave ends with a push + DoD verification
> gate. If any DoD fails, diagnose, patch, re-test, re-commit until green.
>
> **Context:** This prompt implements the Blueprint Orchestrator v1.0 spec
> (`BLUEPRINT_ORCHESTRATOR_V1.md`). The v1.2.0 hardening (Waves U-Z) is
> partially complete (Wave U done, Wave V code written). This prompt picks
> up from the Blueprint Orchestrator design + builds the full project plane
> on top of the existing gateway.
>
> **Existing state:** 22 DB tables, 18 route modules, 6 crypto modules,
> workflow engine (executor + DAG), 9 images in the registry, WebAuthn E2E
> working, SDK with --path support, rocky-base with pre-installed SDK.
> Dependencies: Rust 1.97.1, k3s v1.36.2, buildah 1.43.2, cosign, syft,
> grype, gh, all provisioned.

---

## 0. Pre-Flight (orchestrator-only)

Before any wave begins:

1. **Verify** the dev box (45.63.97.103):
   ```bash
   python3 /home/z/my-project/scripts/ssh_exec.py 'kubectl get nodes; systemctl status stronghold-gateway | head -3; curl -sk -o /dev/null -w "%{http_code}" https://localhost:8443/agent/health; curl -sk https://localhost:8443/admin/images | jq ".repositories | length"; rustc --version; cargo --version; which cosign syft grype gh buildah'
   ```
   All must pass: 1+ Ready node, gateway active, http=200, ≥9 images, Rust 1.97+, all tools present.

2. **Read** `/root/stronghold/worklog.md` for prior context.

3. **Record** the starting commit SHA:
   ```bash
   python3 /home/z/my-project/scripts/ssh_exec.py 'cd /root/stronghold && git log --oneline -1'
   ```

4. **Append** a `---` separator + `Task ID: blueprint-orchestrator-impl` header to the worklog.

5. **Install** any missing crates (latest stable):
   ```bash
   # These will be added to gateway/Cargo.toml as needed per wave:
   # - serde_yaml (front-matter parsing)
   # - pulldown-cmark or comrak (markdown AST)
   # - uuid (agent UUID generation — already a dep via ulid, but UUID format differs)
   ```

Only after all pre-flight checks pass may Wave BA begin.

---

## WAVE BA: Project Model + Concurrent Phase Scheduler

**Goal:** Create the project plane — onboard a repo, scan for blueprint docs, track concurrent phase states, advance phases based on ratings.

**Context budget:** 5 sub-agent tasks.

### BA1 (sub-agent): Projects table + migration
- **Files:** `gateway/src/db/mod.rs`, `gateway/src/db/schema.sql`
- **Task:** Add migration 007 creating `projects` + `project_phase_states` + `phase_transitions` tables (schemas in `BLUEPRINT_ORCHESTRATOR_V1.md` §12). Add `Project` struct + CRUD helpers in a new `gateway/src/projects/mod.rs` module.
- **DoD:**
  - `cargo build --bin stronghold-gateway --features no-sev-snp` compiles
  - `cargo test --features no-sev-snp --lib db` passes (including new migration test)
  - Migration 007 runs cleanly on a fresh DB + on the existing dev DB
- **Commit:** `feat(projects): projects + phase_states + transitions tables (BA1)`

### BA2 (sub-agent): Project onboarding endpoint
- **Files:** `gateway/src/routes/projects.rs` (new), `gateway/src/routes/mod.rs`
- **Task:** Implement `POST /projects` that: (1) clones the repo to `/var/lib/stronghold/repos/<project_id>/`, (2) scans `docs/blueprint/` for the 7 documents, (3) creates a `projects` row, (4) creates `project_phase_states` rows (status=active for BOOTSTRAP if docs missing, status=pending for all phases), (5) returns the project state.
- **DoD:**
  - `POST /projects` with `{"repo_url":"https://github.com/pkhairkh/stronghold.git","name":"test"}` → 200 with project_id
  - Repo is cloned to `/var/lib/stronghold/repos/<id>/`
  - `project_phase_states` has 8 rows (one per phase, all pending except BOOTSTRAP=active)
  - Audit entry `project_created` written (dual-signed)
- **Commit:** `feat(projects): POST /projects onboarding endpoint (BA2)`

### BA3 (sub-agent): Project state queries
- **Files:** `gateway/src/routes/projects.rs`
- **Task:** Implement `GET /projects` (list), `GET /projects/:id` (full state with phase_states + latest ratings), `GET /projects/:id/documents` (list blueprint docs found during scan).
- **DoD:**
  - `GET /projects` returns array of projects with id, name, phase, status
  - `GET /projects/:id` returns project + phase_states array + documents array
  - `GET /projects/:id/documents` returns the 7 blueprint paths + whether they exist
- **Commit:** `feat(projects): project state query endpoints (BA3)`

### BA4 (sub-agent): PhaseScheduler service
- **Files:** `gateway/src/projects/phase_scheduler.rs` (new), `gateway/src/projects/mod.rs`
- **Task:** Implement `compute_active_phases(project_id) -> Vec<ActivePhase>` that reads `project_phase_states` + applies the dependency rules from §3.3 of the v1.0 spec. When a phase's rating passes, the scheduler checks if downstream phases can start (e.g., ADRs + fine_draft both depend on rough_draft; when rough_draft passes, both become active).
- **DoD:**
  - `compute_active_phases` returns the correct active set for: (a) BOOTSTRAP only, (b) problem_catalog + rough_draft concurrent, (c) adrs + fine_draft concurrent, (d) spec (requires both adrs + fine_draft), (e) tasks, (f) progress
  - Unit tests: `test_bootstrap_only`, `test_concurrent_adrs_fine_draft`, `test_spec_requires_both`
- **Commit:** `feat(projects): PhaseScheduler — concurrent phase computation (BA4)`

### BA5 (sub-agent): Phase transition + force-advance
- **Files:** `gateway/src/routes/projects.rs`, `gateway/src/projects/mod.rs`
- **Task:** Implement `advance_phase(project_id, from_phase, to_phase, trigger, rating_id)` that: (1) validates the transition is legal (dependency rules), (2) updates `project_phase_states`, (3) writes a `phase_transitions` audit row, (4) calls `PhaseScheduler` to activate downstream phases. Also implement `POST /projects/:id/force-advance` (owner-only, audited as `force_advance`).
- **DoD:**
  - `advance_phase` correctly transitions: problem_catalog → rough_draft → (adrs + fine_draft concurrent) → spec → tasks → progress
  - Illegal transitions (e.g., problem_catalog → spec) return an error
  - `POST /projects/:id/force-advance` works (owner token required)
  - `phase_transitions` table records every transition with trigger + rating_id
  - Unit test: `test_legal_transitions`, `test_illegal_transitions`, `test_force_advance`
- **Commit:** `feat(projects): phase transition logic + force-advance (BA5)`

### Wave BA DoD Gate
- `cargo test --features no-sev-snp --lib projects` → all pass
- `curl -sk -X POST https://localhost:8443/projects -H "Content-Type: application/json" -d '{"repo_url":"https://github.com/pkhairkh/stronghold.git","name":"ba-test"}'` → 200
- `curl -sk https://localhost:8443/projects` → array with the new project
- `curl -sk https://localhost:8443/projects/<id>` → phase_states with BOOTSTRAP active
- `git push origin main`
- Append `Wave BA: PASS` to worklog

---

## WAVE BB: Generic Agent Connection + Allocator

**Goal:** Let free-tier z.ai agents connect with a UUID, get phone-approved, and get assigned to a project by the AgentAllocator.

**Context budget:** 5 sub-agent tasks.

### BB1 (sub-agent): project_agents + agent_pool tables
- **Files:** `gateway/src/db/mod.rs`
- **Task:** Add migration 008 creating `project_agents` + `agent_pool` + `owner_context` tables (schemas in §12 of v1.0 spec).
- **DoD:**
  - Migration 008 runs cleanly
  - `cargo test --features no-sev-snp --lib db` passes
- **Commit:** `feat(agents): project_agents + agent_pool tables (BB1)`

### BB2 (sub-agent): POST /agent/connect
- **Files:** `gateway/src/routes/agent_connect.rs` (new), `gateway/src/routes/mod.rs`
- **Task:** Implement `POST /agent/connect` that: (1) validates UUID format, (2) checks for reconnection (existing agent by UUID), (3) creates `pending_agent` record, (4) pushes WebAuthn approval to the project owner's phone, (5) long-polls for decision (60s), (6) on approval: calls AgentAllocator, (7) returns assignment or pooled status.
- **DoD:**
  - `POST /agent/connect` with valid UUID → 200 (after phone approval)
  - Reconnection with same UUID → resume (returns existing assignment if machine alive)
  - Phone denial → `{"status":"denied"}`
  - Timeout → `{"status":"denied","reason":"Approval timed out."}`
  - Audit: `agent_connected`, `agent_approved`, `agent_assigned` or `agent_pooled`
- **Commit:** `feat(agents): POST /agent/connect — generic agent connection (BB2)`

### BB3 (sub-agent): AgentAllocator service
- **Files:** `gateway/src/projects/agent_allocator.rs` (new)
- **Task:** Implement `allocate_agent(agent_uuid, capabilities, preferred_roles) -> Option<Assignment>` using the weighted priority algorithm from §6.2: project.priority × 0.40 + project.urgency × 0.25 + project.stake × 0.20 + agent.time_in_pool × 0.15. Compute urgency from blocked-task ratio + ETA risk + phase staleness.
- **DoD:**
  - When 2 projects need coders, the higher-priority project gets the next coder
  - When a project has 0 priority and another has 10, the 10-priority project always wins
  - Urgency computed correctly (project with 5 blocked tasks has higher urgency than one with 0)
  - Unit tests: `test_priority_weighting`, `test_urgency_computation`, `test_fairness_bonus`
- **Commit:** `feat(agents): AgentAllocator — weighted priority allocation (BB3)`

### BB4 (sub-agent): Agent pool + demand signaling
- **Files:** `gateway/src/routes/agent_connect.rs`, `gateway/src/projects/agent_allocator.rs`
- **Task:** Implement the pool: when no project needs an agent, insert into `agent_pool` with 30min TTL. Implement `GET /agent/demand` that returns active role demand across all projects. Implement the pool auto-expire background task (runs every 60s, expires pooled agents past TTL).
- **DoD:**
  - Agent with no matching project → `{"status":"pooled","pool_position":N}`
  - `GET /agent/demand` returns `[{role, projects, urgency, capabilities_needed}]`
  - Pooled agents auto-expire after 30min (verified by checking DB after TTL)
  - Unit test: `test_pool_ttl_expiry`
- **Commit:** `feat(agents): agent pool + demand signaling + TTL expiry (BB4)`

### BB5 (sub-agent): Bootstrap prompt + SDK download
- **Files:** `gateway/src/routes/agent_connect.rs`
- **Task:** Implement `GET /agent/bootstrap-prompt` (returns the markdown from §11 of v1.0 spec with `<gateway>` replaced). Implement `GET /agent/stronghold-agent.sh` (serves the SDK file). Implement `GET /agent/:uuid/status` (returns agent state). Implement `POST /agent/:uuid/release` (releases agent to pool).
- **DoD:**
  - `GET /agent/bootstrap-prompt` returns valid markdown with the gateway URL
  - `GET /agent/stronghold-agent.sh` returns the SDK (200, text/x-shellscript)
  - `GET /agent/:uuid/status` returns `{uuid, state, project_id, role}`
  - `POST /agent/:uuid/release` moves agent to pooled state
- **Commit:** `feat(agents): bootstrap prompt + SDK download + status + release (BB5)`

### Wave BB DoD Gate
- `cargo test --features no-sev-snp --lib agents` → all pass
- Bootstrap prompt fetchable + contains gateway URL
- Demand endpoint returns active demand
- `git push origin main`
- Append `Wave BB: PASS` to worklog

---

## WAVE BC: Document Parser + Rating Pipeline

**Goal:** Parse blueprint documents (front-matter, checkboxes, cross-references, coverage) and rate them through the 5-stage pipeline.

**Context budget:** 5 sub-agent tasks.

### BC1 (sub-agent): DocumentParser
- **Files:** `gateway/src/projects/document_parser.rs` (new)
- **Task:** Implement `parse(content: &str) -> ParsedDocument` that: (1) parses YAML front-matter (using `serde_yaml`), (2) walks ATX headings into a section map, (3) extracts checkboxes with states (`[ ]`, `[~]`, `[x]`, `[!]`, `[-]`), (4) extracts `[[ID]]` cross-references, (5) parses task lines with pipe-delimited metadata (`role:`, `est:`, `dep:`, `implements:`), (6) computes a `CoverageReport` (which IDs are referenced where, what's missing).
- **DoD:**
  - Parse the example `tasks.md` from §4.5 of v1.0 spec → correct checkbox states, links, tasks, coverage
  - Parse a problem catalog → correct `[[P-NNN]]` extraction
  - Parse a doc with broken links → `broken_links` populated
  - Unit tests: `test_parse_front_matter`, `test_parse_checkboxes`, `test_parse_cross_references`, `test_parse_task_metadata`, `test_coverage_report`
- **Commit:** `feat(rating): DocumentParser — front-matter + checkboxes + links + coverage (BC1)`

### BC2 (sub-agent): Objective metrics computation
- **Files:** `gateway/src/projects/document_parser.rs`
- **Task:** Implement `compute_objective_metrics(parsed: &ParsedDocument) -> ObjectiveMetrics` that computes: coverage %, missing sections, broken links count, checkbox progress %, word count, section count, dependency cycle detection (for tasks doc).
- **DoD:**
  - Coverage % = (addressed / total) × 100
  - Missing sections detected (required sections per doc type, absent)
  - Dependency cycle detection returns the cycle if found
  - Unit tests with known documents
- **Commit:** `feat(rating): objective metrics — coverage, missing sections, cycles (BC2)`

### BC3 (sub-agent): document_ratings table + rating storage
- **Files:** `gateway/src/db/mod.rs`, `gateway/src/projects/rating_store.rs` (new)
- **Task:** Add migration 009 creating `document_ratings` + `orchestrator_calibration` tables. Implement `store_rating`, `get_rating`, `get_rating_history` (last 20 for a tenant), `get_calibration`, `update_calibration`.
- **DoD:**
  - Migration 009 runs cleanly
  - `store_rating` + `get_rating` round-trip works
  - `get_rating_history` returns last 20 ordered by created_at DESC
- **Commit:** `feat(rating): document_ratings table + storage + history (BC3)`

### BC4 (sub-agent): LLM rater (z-ai-web-dev-sdk)
- **Files:** `gateway/src/projects/llm_rater.rs` (new)
- **Task:** Implement `rate_document(parsed, rubric, history, calibration) -> LlmRating` that calls the LLM via the `LLM` skill (z-ai-web-dev-sdk). The prompt includes: the parsed document structure, objective metrics, rubric, rating history (last 20), calibration notes. The LLM returns JSON `{score, criteria: [{criterion, score, feedback}], overall_feedback, revision_instructions}`. Use temperature=0 for determinism.
- **DoD:**
  - `rate_document` returns a valid `LlmRating` for a sample problem catalog
  - Same input → same output (deterministic at temp=0)
  - The prompt includes rating history + calibration
  - Unit test with a mocked LLM response
- **Commit:** `feat(rating): LLM rater — z-ai-web-dev-sdk integration (BC4)`

### BC5 (sub-agent): Rating pipeline + POST /orchestrator/rate
- **Files:** `gateway/src/projects/rating_pipeline.rs` (new), `gateway/src/routes/orchestrator.rs` (new), `gateway/src/routes/mod.rs`
- **Task:** Implement the 5-stage pipeline: `rate_document(project_id, document_path) -> Rating`. Stage 1: read + parse. Stage 2: objective metrics. Stage 3: LLM rate. Stage 4: aggregate (LLM score minus objective penalties). Stage 5: threshold check + feedback. Wire `POST /orchestrator/rate` + `GET /orchestrator/ratings/:project_id` + `POST /orchestrator/ratings/:id/override` + `GET /orchestrator/rubric/:doc_type`.
- **DoD:**
  - `POST /orchestrator/rate` with a real document → 200 with score, criteria, feedback
  - Objective penalties applied (unaddressed problems → -5 each)
  - `POST /orchestrator/ratings/:id/override` changes the score + logs the override
  - `GET /orchestrator/rubric/problem_catalog` returns the rubric JSON
  - Integration test: rate a sample problem catalog → score + feedback
- **Commit:** `feat(rating): 5-stage pipeline + /orchestrator/rate + override (BC5)`

### Wave BC DoD Gate
- `cargo test --features no-sev-snp --lib rating` → all pass
- `curl -sk -X POST https://localhost:8443/orchestrator/rate -d '{"project_id":"<id>","document_path":"docs/blueprint/01-problem-catalog.md"}'` → 200 with score
- `git push origin main`
- Append `Wave BC: PASS` to worklog

---

## WAVE BD: Document Convention + Locks + Submission

**Goal:** Document templates, lock protocol, submission endpoint, conflict resolver.

**Context budget:** 4 sub-agent tasks.

### BD1 (sub-agent): Document templates
- **Files:** `gateway/src/projects/document_templates.rs` (new), `templates/blueprint/` (new directory with 7 template files)
- **Task:** Create markdown templates for all 7 document types with YAML front-matter, required headings, checkbox examples, `[[ID]]` examples. Implement `get_template(doc_type) -> String` that returns the template.
- **DoD:**
  - 7 template files exist in `templates/blueprint/`
  - Each template has valid YAML front-matter + all required sections
  - `get_template("problem_catalog")` returns the template
  - Templates are parseable by DocumentParser
- **Commit:** `feat(docs): 7 blueprint document templates (BD1)`

### BD2 (sub-agent): Document locks
- **Files:** `gateway/src/db/mod.rs`, `gateway/src/projects/document_locks.rs` (new), `gateway/src/routes/projects.rs`
- **Task:** Add migration 010 creating `document_locks` table. Implement `acquire_lock`, `release_lock`, `check_lock`. Wire `POST /projects/:id/documents/:path/lock` + `POST /projects/:id/documents/:path/unlock`. Locks auto-expire after 5min (background task checks every 60s).
- **DoD:**
  - Acquire lock with correct `expected_version` → 200
  - Acquire lock with stale version → 409 conflict
  - Acquire lock when already locked by another agent → 409
  - Lock auto-expires after 5min
  - Unit tests: `test_acquire_release`, `test_stale_version_conflict`, `test_double_lock_conflict`
- **Commit:** `feat(docs): document lock protocol with optimistic versioning (BD2)`

### BD3 (sub-agent): Document submission + conflict resolver
- **Files:** `gateway/src/routes/projects.rs`, `gateway/src/projects/conflict_resolver.rs` (new)
- **Task:** Implement `POST /projects/:id/documents/:path/submit` that: (1) checks the agent holds the lock, (2) writes the document to the repo + DB, (3) triggers the rating pipeline, (4) releases the lock. Implement `ConflictResolver` that detects version conflicts + returns structured error with `current_version` + `resolution: "re_read_and_merge"`.
- **DoD:**
  - Submit with lock → 200, rating triggered
  - Submit without lock → 403
  - Submit with stale version → 409 with conflict details
  - Rating result returned in the response (or async via polling)
- **Commit:** `feat(docs): document submission + conflict resolver (BD3)`

### BD4 (sub-agent): Document query endpoints
- **Files:** `gateway/src/routes/projects.rs`
- **Task:** Implement `GET /projects/:id/documents` (list all 7 docs + existence + latest rating), `GET /projects/:id/documents/:path` (get content + latest rating + rating history).
- **DoD:**
  - `GET /projects/:id/documents` returns array of 7 docs with `{path, exists, latest_rating}`
  - `GET /projects/:id/documents/01-problem-catalog.md` returns content + rating + history
- **Commit:** `feat(docs): document query endpoints (BD4)`

### Wave BD DoD Gate
- `cargo test --features no-sev-snp --lib docs` → all pass
- Lock + submit + rate flow works end-to-end on a sample document
- `git push origin main`
- Append `Wave BD: PASS` to worklog

---

## WAVE BE: Reprompt Injection

**Goal:** The RepromptInjector service, per-role templates, reprompt queue, heartbeat loop.

**Context budget:** 4 sub-agent tasks.

### BE1 (sub-agent): Reprompt queue + delivery
- **Files:** `gateway/src/db/mod.rs`, `gateway/src/projects/reprompt.rs` (new)
- **Task:** Add migration 011 creating `reprompt_queue` table. Implement `enqueue_reprompt`, `dequeue_reprompt` (FIFO with priority), `deliver_reprompt` (injects via PTY or control WS). Implement `GET /agent/:uuid/reprompt/next` (agent polls for its next reprompt).
- **DoD:**
  - Enqueue 3 reprompts → dequeue returns highest priority first
  - `GET /agent/:uuid/reprompt/next` returns the next reprompt or 204 if empty
  - Delivered reprompts marked with `delivered_at`
- **Commit:** `feat(reprompt): queue + priority delivery + polling endpoint (BE1)`

### BE2 (sub-agent): Reprompt composition + per-role templates
- **Files:** `gateway/src/projects/reprompt.rs`
- **Task:** Implement `compose_reprompt(agent, project, trigger) -> Reprompt` (from §5.5 of v1.0 spec). Implement per-role templates: spec_writer, coder, tester, reviewer, watchdog, architect, planner, integrator, oracle, facilitator. Each template follows the universal structure (IDENTITY → ROLE → PROJECT → TASK → CONTEXT → INSTRUCTION → SDK → CONSTRAINTS).
- **DoD:**
  - `compose_reprompt` for a coder agent → contains all 8 sections
  - Role-specific constraints present (coder: "Don't push to main", spec_writer: "Don't write code")
  - Context includes latest feedback + recent messages
  - Unit tests for each role template
- **Commit:** `feat(reprompt): composition + 10 per-role templates (BE2)`

### BE3 (sub-agent): Reprompt triggers + heartbeat loop
- **Files:** `gateway/src/projects/reprompt.rs`
- **Task:** Implement the trigger logic: turn_start (after each agent result), phase_change (when project phase advances), feedback (when rating < threshold), message (when a message arrives on the project bus), reassign (when agent gets a new task), heartbeat (every 60s for active agents). The heartbeat runs as a background tokio task.
- **DoD:**
  - Agent submits a result → turn_start reprompt enqueued
  - Phase advances → phase_change reprompt enqueued for all active agents
  - Rating < threshold → feedback reprompt enqueued for the producing agent
  - Heartbeat enqueues a reprompt every 60s for agents in working state
- **Commit:** `feat(reprompt): triggers + 60s heartbeat loop (BE3)`

### BE4 (sub-agent): Reprompt injection via PTY + control WS
- **Files:** `gateway/src/projects/reprompt.rs`, `gateway/src/routes/instruct.rs`
- **Task:** Implement `inject_via_pty(machine_id, reprompt)` that writes the reprompt block to the PTY stdin (wrapped in `STRONGHOLD_REPROMPT` markers). Implement `inject_via_control(machine_id, reprompt)` that sends a JSON envelope on the control WebSocket. Wire `POST /agent/:uuid/reprompt/inject` (orchestrator-only).
- **DoD:**
  - PTY injection: the reprompt text appears in the pod's PTY
  - Control WS injection: the JSON envelope is sent on the WebSocket
  - `POST /agent/:uuid/reprompt/inject` enqueues + delivers
- **Commit:** `feat(reprompt): PTY + control WS injection channels (BE4)`

### Wave BE DoD Gate
- `cargo test --features no-sev-snp --lib reprompt` → all pass
- Heartbeat loop running (verified by checking reprompt_queue after 60s)
- `git push origin main`
- Append `Wave BE: PASS` to worklog

---

## WAVE BF: Re-Spec + Human-in-the-Loop

**Goal:** Re-spec economics, owner privileges, owner context injection, notifications.

**Context budget:** 4 sub-agent tasks.

### BF1 (sub-agent): Re-spec economics
- **Files:** `gateway/src/db/mod.rs`, `gateway/src/projects/respec.rs` (new)
- **Task:** Add migration 012 creating `respec_events` table. Implement `compute_respec_cost(project_id, from_phase, to_phase) -> RespecCost` (tasks invalidated, agents to reassign, docs to re-rate, estimated delay). Implement `trigger_respec(project_id, trigger, trigger_agent) -> RespecResult` that calls the facilitator LLM to approve/deny based on cost.
- **DoD:**
  - Cost model correctly counts invalidated tasks
  - Facilitator approves low-cost re-specs, denies high-cost ones
  - `respec_budget` decremented on approval
  - `respec_events` table records every re-spec attempt
- **Commit:** `feat(respec): cost model + facilitator approval + budget (BF1)`

### BF2 (sub-agent): Owner privileges
- **Files:** `gateway/src/routes/projects.rs`
- **Task:** Implement `POST /projects/:id/force-advance`, `POST /projects/:id/force-rollback` (consumes respec budget), `POST /projects/:id/pause`, `POST /projects/:id/resume`, `POST /projects/:id/archive`, `POST /projects/:id/priority`. All require the tenant owner token.
- **DoD:**
  - Force-advance works (bypasses rating, audited)
  - Force-rollback consumes respec budget
  - Pause releases all agents to pool
  - Resume reactivates the project
  - Archive marks status=archived + releases agents
- **Commit:** `feat(owner): force-advance/rollback/pause/resume/archive/priority (BF2)`

### BF3 (sub-agent): Owner context injection
- **Files:** `gateway/src/db/mod.rs`, `gateway/src/routes/projects.rs`, `gateway/src/projects/reprompt.rs`
- **Task:** Add migration 013 creating `owner_context` table. Implement `POST /projects/:id/context` (owner adds a note). Update `compose_reprompt` to include `### OWNER CONTEXT` section with all active owner notes for the project.
- **DoD:**
  - `POST /projects/:id/context` with `{"context":"Must support mTLS"}` → 200
  - Subsequent reprompts for agents on that project include the owner context
  - Multiple context notes accumulate (all injected)
- **Commit:** `feat(owner): context injection into reprompts (BF3)`

### BF4 (sub-agent): Owner notifications
- **Files:** `gateway/src/projects/notifications.rs` (new)
- **Task:** Implement push notifications (via ntfy) to the project owner for: phase transitions, ratings below threshold, re-spec requests (high-cost), watchdog alerts (dedication < 0.3), project health changes. Each notification includes the project name + a summary.
- **DoD:**
  - Phase transition → notification pushed
  - Rating < threshold → notification pushed
  - Watchdog dedication < 0.3 → notification pushed
  - Notifications have the project name + a 1-line summary
- **Commit:** `feat(owner): ntfy notifications for phase/rating/respec/watchdog (BF4)`

### Wave BF DoD Gate
- `cargo test --features no-sev-snp --lib respec` → all pass
- `cargo test --features no-sev-snp --lib owner` → all pass
- Force-advance + pause + resume flow works
- Owner context appears in reprompts
- `git push origin main`
- Append `Wave BF: PASS` to worklog

---

## WAVE BG: E2E Integration Test + Bootstrap

**Goal:** Full project lifecycle test + bootstrap prompt serving + agent performance ratings.

**Context budget:** 4 sub-agent tasks.

### BG1 (sub-agent): Agent performance ratings
- **Files:** `gateway/src/db/mod.rs`, `gateway/src/projects/agent_performance.rs` (new)
- **Task:** Add migration 014 creating `agent_performance_ratings` table. Implement `rate_agent_performance(agent_uuid, project_id) -> PerformanceRating` that computes: dedication (avg watchdog), output_quality (avg doc/code ratings), timeliness (estimate vs actual), communication (progress reports + message bus activity). Called when an agent is released.
- **DoD:**
  - `rate_agent_performance` returns a valid score (0-100)
  - Score stored in `agent_performance_ratings`
  - Score follows the agent UUID (queryable across projects)
- **Commit:** `feat(agents): performance ratings — dedication + quality + timeliness + comms (BG1)`

### BG2 (sub-agent): E2E test script
- **Files:** `scripts/blueprint_e2e.sh` (new)
- **Task:** Write a full E2E test that: (1) creates a project (onboard a test repo), (2) verifies BOOTSTRAP phase, (3) submits a problem catalog, (4) triggers rating, (5) verifies phase advance to rough_draft, (6) simulates an agent connecting via `/agent/connect` (with mock phone approval), (7) verifies agent assigned to the project, (8) submits all 7 docs through the pipeline, (9) verifies phase = DONE, (10) verifies audit trail.
- **DoD:**
  - `bash scripts/blueprint_e2e.sh` exits 0
  - All 7 documents submitted + rated
  - Phase transitions: BOOTSTRAP → problem_catalog → ... → progress → DONE
  - Audit log has `project_created`, `document_rated`, `phase_advanced` × 7, `project_completed`
- **Commit:** `test(blueprint): full E2E lifecycle test (BG2)`

### BG3 (sub-agent): Bootstrap prompt serving + well-known URL
- **Files:** `gateway/src/routes/agent_connect.rs`, `gateway/src/routes/mod.rs`
- **Task:** Serve the bootstrap prompt at `GET /agent/bootstrap-prompt` + `GET /.well-known/stronghold-agent`. Serve the SDK at `GET /agent/stronghold-agent.sh`. The bootstrap prompt includes the actual gateway URL (from the request Host header or a configured base URL).
- **DoD:**
  - `GET /agent/bootstrap-prompt` returns the markdown with the correct gateway URL
  - `GET /.well-known/stronghold-agent` returns the same
  - `GET /agent/stronghold-agent.sh` returns the SDK (200, executable)
- **Commit:** `feat(bootstrap): serve bootstrap prompt + SDK + well-known URL (BG3)`

### BG4 (orchestrator-only): Full integration run + commit
- **Task:** Deploy the gateway with all Blueprint Orchestrator code. Run the E2E test. Verify the full flow. Run the existing holistic test (49/49) + deep test (35/35) to ensure no regressions. Tag the release.
- **DoD:**
  - `bash scripts/blueprint_e2e.sh` → exit 0
  - `bash scripts/holistic_test.sh` → 49/49 pass
  - `bash scripts/deep_test.sh` → 35/35 pass
  - `git tag v1.3.0-blueprint -m "Blueprint Orchestrator — project plane + rating pipeline + reprompt injection"`
  - `git push origin main && git push origin v1.3.0-blueprint`
- **Commit:** (the tag is the commit; no separate commit needed)

### Wave BG DoD Gate
- `bash scripts/blueprint_e2e.sh` → exit 0
- `bash scripts/holistic_test.sh` → 49/49
- `bash scripts/deep_test.sh` → 35/35
- `git tag --list v1.3.0-blueprint` → present
- `git push origin main && git push origin v1.3.0-blueprint`
- Append `Wave BG: PASS — v1.3.0-blueprint tagged` to worklog

---

## ORCHESTRATOR RETURN CONTRACT

The orchestrator may return only when ALL of the following are true:

1. ✅ Wave BA DoD: Projects created, phases concurrent, transitions work
2. ✅ Wave BB DoD: Agents connect, get approved, get assigned by allocator
3. ✅ Wave BC DoD: Documents parsed, rated through 5-stage pipeline
4. ✅ Wave BD DoD: Document locks work, submission triggers rating
5. ✅ Wave BE DoD: Reprompts enqueued, composed per-role, injected via PTY/WS
6. ✅ Wave BF DoD: Re-spec economics work, owner privileges functional
7. ✅ Wave BG DoD: E2E test passes, v1.3.0-blueprint tagged
8. ✅ All commits pushed to `origin/main`
9. ✅ Worklog updated with per-wave PASS entries
10. ✅ Existing tests (holistic 49/49, deep 35/35) still pass — no regressions

**If any DoD fails:** Diagnose, patch, re-test, re-commit, re-push. The orchestrator does not return on red.

---

## SUB-AGENT CONTEXT WINDOW RULES

- Each sub-agent task ≤ 500 lines of code change
- Sub-agents read ONLY: their task description, the files they modify, the v1.0 spec section referenced, + the previous wave's worklog entry
- Sub-agents append to `/root/stronghold/worklog.md` before returning
- The orchestrator spawns sub-agents via `Task` tool with `subagent_type: "general-purpose"` + a self-contained prompt
- Sub-agents use `python3 /home/z/my-project/scripts/ssh_exec.py '<command>'` for all dev box commands

---

## DEPENDENCY PROVISIONING (latest stable)

Already provisioned (verify, don't reinstall):
- Rust 1.97.1, k3s v1.36.2, buildah 1.43.2, cosign, syft, grype, gh

New crates to add (per wave, latest stable):
- `serde_yaml` — YAML front-matter parsing (Wave BC)
- `comrak` — markdown AST parsing (Wave BC, alternative: `pulldown-cmark`)
- `uuid` — UUID v4 generation for agent UUIDs (Wave BB, if not already present)

Install via `cargo add <crate>` in `gateway/Cargo.toml`. Never use pre-release versions.

---

## COMMITS + PUSH CADENCE

- **Per task:** one conventional-commit (`feat(scope): ...`, `fix(scope): ...`, `test(scope): ...`, `docs(scope): ...`)
- **Per wave:** one `git push origin main` after the DoD gate passes
- **Per wave:** one worklog entry with `Wave <letters>: PASS` or `Wave <letters>: FAIL (reason)`
- **Final:** `git tag v1.3.0-blueprint` + `git push origin v1.3.0-blueprint`

The orchestrator MUST NOT batch commits across waves. Each wave's push is independent.
