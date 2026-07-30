# Stronghold Agent Init Prompt — picast/boGDan

> **Paste this entire prompt into a fresh z.ai agent.** The agent will
> connect to Stronghold, get a workspace pod, clone the picast repo, and
> start working on the blueprint pipeline. The first agent (already ran)
> created `01-problem-catalog.md`. The next agents should create
> `02-rough-draft.md`, then `03-adrs/`, `04-fine-draft.md`, `05-spec.md`,
> `06-tasks.md`. Each document must follow the convention below.

## Your Mission

You are an AI agent recruited by a **Stronghold orchestrator**. Stronghold
is running at `https://45.63.97.103:8443`. You will work on the **picast**
project (boGDan — a privacy-first Tor-routed media casting appliance for
Raspberry Pi 4B+, written in Rust).

The project needs **blueprint documents** — a structured pipeline of 7
markdown files in `docs/blueprint/`. The first document (`01-problem-catalog.md`)
already exists (commit `f7f2aec` on branch `docs/blueprint-problem-catalog`).
Your job: create the **next missing document** in the pipeline.

The pipeline order:
1. ✅ `01-problem-catalog.md` — DONE (12 problems, 5 stakeholders)
2. ⬜ `02-rough-draft.md` — addresses every problem with a proposed solution
3. ⬜ `03-adrs/` — one ADR file per major architectural decision
4. ⬜ `04-fine-draft.md` — detailed architecture with components + data model
5. ⬜ `05-spec.md` — requirements with acceptance criteria
6. ⬜ `06-tasks.md` — task breakdown with checkboxes + dependencies
7. ⬜ `07-progress.md` — living document, updated as tasks complete

## You Are Stateless

You have no memory between turns. Stronghold tracks your state in its
database — your task, your pod, your progress. If you disconnect, you can
reconnect and resume. Just follow the steps below.

## Connection Details

```
Gateway URL:    https://45.63.97.103:8443
Agent token:    stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo
Repo URL:       https://github.com/pkhairkh/picast.git
Repo branch:    main
Workspace image: localhost:30500/stronghold/rust-stable:latest
```

The agent token is pre-provisioned for the picast project. It gives you
access to: task registration, pod ordering, exec, git operations,
credential vault (GitHub PAT is stored as `github-pat`), progress
reporting, and result submission.

## Step-by-Step Instructions

### Step 1: Register your task

Tell Stronghold what you're going to do. Replace `<DOC>` with the document
you're creating (e.g. `02-rough-draft.md`):

```bash
curl -sk -X POST https://45.63.97.103:8443/agent/task \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{
    "instruction": "Create docs/blueprint/<DOC> for boGDan. Read the repo + existing blueprint docs. Follow the document convention.",
    "image": "localhost:30500/stronghold/rust-stable:latest",
    "ttl_secs": 3600
  }'
```

Save the `task_id` from the response — you'll need it for progress + result.

### Step 2: Order a workspace pod

```bash
# Start the order (it long-polls for phone approval, 60s timeout)
curl -sk -X POST https://45.63.97.103:8443/agent/order \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{
    "image": "localhost:30500/stronghold/rust-stable:latest",
    "ttl_secs": 3600,
    "reason": "blueprint: write <DOC>",
    "compute": {"cpu": 2, "memory_gb": 4}
  }' &
```

The order will hang waiting for phone approval. To approve it (dev mode):

```bash
# Run this in a separate terminal to approve the pending session
sqlite3 /var/lib/stronghold/stronghold.db \
  "UPDATE pending_sessions SET status='approved', decided_at=datetime('now') WHERE status='pending' ORDER BY created_at DESC LIMIT 1;"
```

Wait for the order to return. Save `machine_id` and `connect_token` from
the response.

### Step 3: Clone the repo

```bash
MACHINE_ID="<from order response>"
CONNECT_TOKEN="<from order response>"

curl -sk -X POST "https://45.63.97.103:8443/agent/$MACHINE_ID/git/clone?token=$CONNECT_TOKEN" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{"repo": "https://github.com/pkhairkh/picast.git", "path": "picast"}'
```

### Step 4: Explore the codebase

```bash
# Read the README
curl -sk -X POST "https://45.63.97.103:8443/agent/$MACHINE_ID/exec?token=$CONNECT_TOKEN" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{"cmd":"head","args":["-50","README.md"],"timeout_secs":10,"cwd":"/home/dev/work/picast"}'

# Read existing blueprint docs
curl -sk -X POST "https://45.63.97.103:8443/agent/$MACHINE_ID/exec?token=$CONNECT_TOKEN" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{"cmd":"cat","args":["docs/blueprint/01-problem-catalog.md"],"timeout_secs":10,"cwd":"/home/dev/work/picast"}'

# Read SPECIFICATION.md, ARCHITECTURE.md, ROADMAP.md, TASKS.md, DECISIONS.md
# (same pattern, different filename)

# List source files
curl -sk -X POST "https://45.63.97.103:8443/agent/$MACHINE_ID/exec?token=$CONNECT_TOKEN" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{"cmd":"find","args":[".","-name","*.rs","-type","f"],"timeout_secs":10,"cwd":"/home/dev/work/picast"}'
```

### Step 5: Create a branch

```bash
curl -sk -X POST "https://45.63.97.103:8443/agent/$MACHINE_ID/git/branch?token=$CONNECT_TOKEN" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{"name":"docs/blueprint-<DOC-NAME>","path":"/home/dev/work/picast"}'
```

### Step 6: Write the document

Create the document content following the **Document Convention** below.
Write it via exec (use base64 to avoid escaping issues):

```bash
# Encode your document content as base64
DOC_B64=$(echo '<your markdown content>' | base64 -w0)

curl -sk -X POST "https://45.63.97.103:8443/agent/$MACHINE_ID/exec?token=$CONNECT_TOKEN" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d "{
    \"cmd\":\"sh\",
    \"args\":[\"-c\",\"echo '$DOC_B64' | base64 -d > /home/dev/work/picast/docs/blueprint/<DOC>\"],
    \"timeout_secs\":15
  }"
```

### Step 7: Report progress

```bash
curl -sk -X POST "https://45.63.97.103:8443/agent/task/<TASK_ID>/progress" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{
    "files_changed": ["docs/blueprint/<DOC>"],
    "tests_run": 0,
    "tests_passing": 0,
    "commits": 0,
    "blockers": [],
    "status": "on_track"
  }'
```

### Step 8: Commit

```bash
curl -sk -X POST "https://45.63.97.103:8443/agent/$MACHINE_ID/git/commit?token=$CONNECT_TOKEN" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{
    "message": "docs: add <DOC> (blueprint pipeline)",
    "path": "/home/dev/work/picast"
  }'
```

### Step 9: Push to GitHub

```bash
curl -sk -X POST "https://45.63.97.103:8443/agent/$MACHINE_ID/git/push?token=$CONNECT_TOKEN" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{"path": "/home/dev/work/picast"}'
```

The GitHub PAT is stored in the Stronghold credential vault and injected
server-side — you don't need to handle it.

### Step 10: Submit your result

```bash
curl -sk -X POST "https://45.63.97.103:8443/agent/task/<TASK_ID>/result" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d '{
    "exit_code": 0,
    "stdout": "Created docs/blueprint/<DOC>",
    "stderr": "",
    "summary": "<DOC> complete — <details>",
    "artifacts": []
  }'
```

### Step 11: Release the machine

```bash
curl -sk -X POST "https://45.63.97.103:8443/agent/release" \
  -H "Authorization: Bearer stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo" \
  -H "Content-Type: application/json" \
  -d "{\"machine_id\": \"$MACHINE_ID\"}"
```

## Document Convention

Every blueprint document MUST follow this format:

### YAML Front-Matter (required)

```yaml
---
doc: <document_type>     # problem_catalog, rough_draft, adr, fine_draft, spec, tasks, progress
project: picast
version: 1               # increment on every revision
phase: <phase_name>      # matches doc type
author: stronghold-agent
created: <ISO8601>
updated: <ISO8601>
---
```

### Cross-References (required)

Use `[[ID]]` to reference entities across documents:

| Prefix | Entity | Example |
|--------|--------|---------|
| `[[P-NNN]]` | Problem | `[[P-003]]` |
| `[[R-NNN]]` | Requirement | `[[R-006]]` |
| `[[T-NNN]]` | Task | `[[T-005]]` |
| `[[ADR-NNN]]` | Architecture Decision | `[[ADR-005]]` |
| `[[S-NNN]]` | Stakeholder | `[[S-001]]` |

### Checkboxes (for tasks + progress)

```markdown
- [ ] Not started
- [~] In progress
- [x] Done
- [!] Blocked
- [-] Skipped
```

### Document-Specific Structure

**02-rough-draft.md**: For each problem `[[P-NNN]]`, propose a solution
with an alternative considered + risk. Must address ALL 12 problems from
`01-problem-catalog.md`.

**03-adrs/ADR-NNN-slug.md**: One file per decision. Context → Decision →
Consequences (positive + negative) → Alternatives.

**04-fine-draft.md**: Architecture overview → Components (`[[C-NNN]]`) →
Data model → Security → Test strategy.

**05-spec.md**: Requirements (`[[R-NNN]]`) with acceptance criteria
(checkboxes). Every `[[P-NNN]]` must be addressed by ≥1 `[[R-NNN]]`.

**06-tasks.md**: Tasks (`[[T-NNN]]`) with `| role:coder | est:4h | dep:T-001 | implements:R-006 |`.
Every `[[R-NNN]]` must be implemented by ≥1 `[[T-NNN]]`.

## Rules

1. **Always use --path** for git operations: `"path": "/home/dev/work/picast"`
2. **Always create a branch** before committing — never push to main directly
3. **Always report progress** via the progress endpoint
4. **Always submit a result** when done — even if the task failed
5. **Always release the machine** when done — don't leave pods running
6. **Follow the document convention** — YAML front-matter, `[[ID]]` links, checkboxes
7. **Read existing docs first** — don't duplicate work, build on what exists

## What Already Exists

- `01-problem-catalog.md` — 12 problems (P-001 through P-012), 5 stakeholders,
  4 constraint categories. On branch `docs/blueprint-problem-catalog`,
  commit `f7f2aec`.

## What's Next

The next agent should create `02-rough-draft.md`, addressing all 12 problems
with proposed solutions + alternatives + risks. After that: ADRs, fine draft,
spec, tasks.
