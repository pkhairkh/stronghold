# Multi-Agent Agentic Coding Flow

## Agent Roles

### Planner
- Analyzes task, reads codebase, creates implementation plan and workflow DAG
- Read-only git access, no code writing
- API: exec (read-only), git/status, git/log, task/create, workflow/create

### Coder
- Implements changes according to plan, writes tests, creates PRs
- Write git access with GitHub PAT
- API: exec, git/clone, git/branch, git/commit, git/push, git/pr, credentials/get

### Reviewer
- Reviews diffs, checks for bugs/security/style, approves or requests changes
- Read-only git access
- API: exec (read-only), git/status, git/log

### Tester
- Runs full test suite, reports structured results
- Read-only git access
- API: exec (test commands only), git/status

### Integrator
- Merges approved PRs, runs full CI on main, reports result
- Admin git access
- API: exec, git/pr (merge), credentials/get

## Message Bus Protocol

Channel: `workflow-run-<run_id>`

```json
{"type":"task_assigned","task_id":"...","step_id":"...","instruction":"...","context":{...}}
{"type":"review_requested","task_id":"...","pr_url":"...","branch":"..."}
{"type":"changes_requested","task_id":"...","issues":[{"file":"...","line":N,"severity":"high","message":"..."}]}
{"type":"review_approved","task_id":"...","pr_url":"..."}
{"type":"test_results","task_id":"...","passed":N,"failed":N,"duration_ms":N}
{"type":"integration_complete","task_id":"...","pr_number":N,"ci_passed":true}
{"type":"escalation","task_id":"...","reason":"...","details":"..."}
```

## Execution Flow

1. Human submits task → Stronghold creates task
2. Planner clones repo, analyzes, creates workflow DAG
3. Human approves workflow on phone
4. Coder implements, tests locally, creates PR
5. Tester runs full test suite
6. Reviewer reviews diff, approves or requests changes
7. Integrator merges PR, runs CI on main
8. Human monitors throughout, can reprompt mid-session

## Failure Handling

- Agent crash → TTL expires, workflow retries (max 3)
- Test failure → routes back to Coder with failure context
- Review rejection → routes back to Coder with issues list
- Merge conflict → Planner creates conflict-resolution sub-task
- Any agent can escalate to human via message bus

## Mid-Session Reprompt

Three modes:
- `pty`: inject text into running PTY
- `control`: send JSON via OSC 51 escape sequence
- `task`: queue sub-task within session

## Credential Protocol

Credentials injected as env vars at pod creation. Never logged. Accessed via `GET /agent/:machine_id/credentials/:name`.
