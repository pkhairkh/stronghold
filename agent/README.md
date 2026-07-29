# Stronghold Agent Bash SDK

A single file you `source` to turn raw `curl` into typed functions for every
Stronghold gateway endpoint — task lifecycle, structured exec, git flow,
credential vault, mid-session reprompt, workflow DAGs, and the interactive
PTY.

```bash
source /path/to/stronghold-agent.sh

export STRONGHOLD_URL=https://gateway.example.com
export STRONGHOLD_TOKEN=<agent bearer token>

task_id=$(stronghold_task "build the crate" "stronghold/rust-nightly:2026.07" 3600 \
          | jq -r .task_id)
stronghold_result "$task_id" 0 "$stdout" "" "build ok"
```

Every public function prints the gateway's JSON response on **stdout** and
returns the curl exit code, so callers can pipe straight to `jq` and branch on
`$?`.

---

## Requirements

| Binary    | Used by                                         |
|-----------|-------------------------------------------------|
| `curl`    | every endpoint function                         |
| `jq`      | every endpoint function (builds the JSON bodies)|
| `websocat`| `stronghold_shell` only                         |

Override any of them with `STRONGHOLD_CURL`, `STRONGHOLD_JQ`,
`STRONGHOLD_WEBSOCAT` if they live outside `PATH`.

---

## Environment variables

| Variable                  | Required | Purpose                                                      |
|---------------------------|----------|--------------------------------------------------------------|
| `STRONGHOLD_URL`          | yes      | Gateway base URL, no trailing slash.                         |
| `STRONGHOLD_TOKEN`        | yes\*    | Agent bearer token. Sent as `Authorization: Bearer <tok>`.   |
| `STRONGHOLD_CONNECT_TOKEN`| no       | Per-machine connect token (from `POST /agent/order`). Sent as `?token=<tok>` for exec/git/PTY. Falls back to `STRONGHOLD_TOKEN` when unset. |
| `STRONGHOLD_CURL_FLAGS`   | no       | Extra flags appended to every curl call (e.g. `--cacert …`). |
| `STRONGHOLD_CURL`         | no       | Path to the `curl` binary.                                   |
| `STRONGHOLD_JQ`           | no       | Path to the `jq` binary.                                     |
| `STRONGHOLD_WEBSOCAT`     | no       | Path to the `websocat` binary.                               |

\* `STRONGHOLD_TOKEN` is required by every bearer endpoint (task, credential,
instruct, workflow). The exec/git/PTY endpoints accept a connect token; if
`STRONGHOLD_CONNECT_TOKEN` is unset the SDK uses `STRONGHOLD_TOKEN` instead,
so a single token is enough in development.

### Two tokens, one gateway

Stronghold distinguishes two credentials:

- **Agent bearer token** (`STRONGHOLD_TOKEN`) — tenant-scoped, used for the
  task lifecycle, credential vault, mid-session reprompt, and workflow
  endpoints. Sent as `Authorization: Bearer <tok>`.
- **Connect token** (`STRONGHOLD_CONNECT_TOKEN`) — issued per machine by
  `POST /agent/order`, used for `exec`, `git/*`, and the PTY WebSocket. Sent
  as `?token=<tok>` in the query string (URL-encoded).

A typical agent flow:

```bash
# 1. Order a machine (bearer token) — returns machine_id + connect_token.
order=$(curl -sS -X POST "$STRONGHOLD_URL/agent/order" \
        -H "Authorization: Bearer $STRONGHOLD_TOKEN" \
        -d '{"image":"stronghold/rust-nightly:2026.07","ttl_secs":3600,"reason":"build"}')
export STRONGHOLD_CONNECT_TOKEN=$(jq -r .connect_token <<<"$order")
machine_id=$(jq -r .machine_id <<<"$order")

# 2. Use the SDK for everything else.
stronghold_exec "$machine_id" cargo 600 -- build --release
```

---

## Functions

### Task lifecycle

#### `stronghold_task` — create a queued task

```
stronghold_task INSTRUCTION IMAGE TTL_SECS [--context JSON] [--parent TASK_ID] [--workflow-run RUN_ID]
```

`POST /agent/task`. Returns `{ task_id, machine_id, status }`.

```bash
stronghold_task "run the test suite" "stronghold/rust-nightly:2026.07" 1800 \
  --context '{"cargo_flags":"--all-features"}'
```

#### `stronghold_result` — submit a task's result

```
stronghold_result TASK_ID EXIT_CODE STDOUT STDERR SUMMARY [--artifacts JSON_ARRAY]
```

`POST /agent/task/:id/result`. `exit_code == 0` marks the task `completed`;
non-zero marks it `failed`.

```bash
stronghold_result "$task_id" 0 "$stdout" "" "tests passed" \
  --artifacts '[{"path":"./target/report"}]'
```

---

### Structured command execution

#### `stronghold_exec` — run a command, get JSON back

```
stronghold_exec MACHINE_ID CMD TIMEOUT_SECS [--cwd DIR] [--env KEY=VAL ...] [-- ARG ...]
```

`POST /agent/:machine_id/exec`. Returns
`{ exit_code, stdout, stderr, duration_ms, audit_seq }`. Everything after
`--` is the command's argv; `--env` may repeat.

```bash
stronghold_exec "$machine_id" cargo 600 --cwd /workspace \
  --env RUSTFLAGS="-D warnings" -- build --release
```

---

### Git flow

All git endpoints run inside the agent's workspace pod. The tenant's stored
git credential is injected server-side — the secret never crosses this API.

#### `stronghold_git_clone`

```
stronghold_git_clone MACHINE_ID REPO [--branch NAME] [--path DIR]
```

`POST /agent/:machine_id/git/clone`.

```bash
stronghold_git_clone "$machine_id" github.com/acme/repo.git --branch develop --path repo
```

#### `stronghold_git_branch`

```
stronghold_git_branch MACHINE_ID NAME [--from REF]
```

`POST /agent/:machine_id/git/branch`. Creates and checks out `NAME`
(`git checkout -b NAME [FROM]`).

#### `stronghold_git_commit`

```
stronghold_git_commit MACHINE_ID MESSAGE [-- FILE ...]
```

`POST /agent/:machine_id/git/commit`. With no `--` files, stages everything
(`git add -A`).

```bash
stronghold_git_commit "$machine_id" "fix: off-by-one" -- src/lib.rs src/tests.rs
```

#### `stronghold_git_push`

```
stronghold_git_push MACHINE_ID [--remote NAME] [--branch NAME]
```

`POST /agent/:machine_id/git/push`. Defaults: `origin`, current `HEAD`.

#### `stronghold_git_pr`

```
stronghold_git_pr MACHINE_ID TITLE BASE HEAD [--body TEXT]
```

`POST /agent/:machine_id/git/pr`. Opens a GitHub PR via the REST API;
requires a stored `github-pat` credential. Returns
`{ pr_number, pr_url, pr_state }`.

```bash
stronghold_git_pr "$machine_id" "Add feature X" main feature-x --body "Implements X."
```

---

### Credentials

#### `stronghold_credential` — fetch + decrypt a named credential

```
stronghold_credential MACHINE_ID NAME
```

`GET /agent/:machine_id/credentials/:name`. The gateway verifies the bearer
token's tenant matches the machine's tenant, decrypts the value, and returns
`{ name, kind, value, env_var, mount_path }`. The value is **never** logged.

```bash
pat=$(stronghold_credential "$machine_id" github-pat | jq -r .value)
```

---

### Mid-session reprompt

#### `stronghold_instruct` — inject a new instruction into a running session

```
stronghold_instruct MACHINE_ID INSTRUCTION [--mode pty|control|task] [--priority low|normal|high] [--context JSON]
```

`POST /agent/:machine_id/instruct`. The original session approval covers the
whole TTL, so no extra phone tap is needed.

- `pty` (default) types the instruction into the live PTY.
- `task` queues a sub-task within the session.
- `control` is reserved for the N4 control WebSocket.

```bash
stronghold_instruct "$machine_id" "also fix the docs" --priority high
```

---

### Interactive PTY

#### `stronghold_shell` — open a PTY over WebSocket

```
stronghold_shell MACHINE_ID [WEBSOCAT_FLAGS ...]
```

Connects `websocat` to `wss://<host>/agent/<machine_id>/pty?token=<connect>`
(text mode by default). Extra arguments are forwarded to `websocat`
(e.g. `-b` for binary). The connect token is URL-encoded automatically.

```bash
stronghold_shell "$machine_id"        # interactive text session
stronghold_shell "$machine_id" -b     # binary / raw mode
```

---

### Workflows

#### `stronghold_workflow_create` — define a workflow

```
stronghold_workflow_create NAME DAG_JSON
```

`POST /workflow`. `DAG_JSON` is a JSON object (`{"steps":[…]}`) stored verbatim
and parsed at run time. Returns `{ workflow_id, status }` (status `draft`).

```bash
stronghold_workflow_create ci-build '{"steps":[{"name":"build"},{"name":"test","after":"build"}]}'
```

#### `stronghold_workflow_run` — start a run

```
stronghold_workflow_run WORKFLOW_ID
```

`POST /workflow/:id/run`. Spawns the engine in the background and returns
immediately with `{ run_id, status }`.

#### `stronghold_workflow_status` — poll a run

```
stronghold_workflow_status RUN_ID
```

`GET /workflow/run/:id`. Returns
`{ id, workflow_id, status, current_steps, completed_steps, started_at, finished_at }`.

```bash
stronghold_workflow_run wf_01ABC     # → { "run_id": "run_02…", "status": "running" }
stronghold_workflow_status run_02…    # poll until status is terminal
```

---

## End-to-end example

```bash
#!/usr/bin/env bash
set -euo pipefail
source stronghold-agent.sh

# 1. Queue a task.
task_id=$(stronghold_task "ship v1.2.3" "stronghold/rust-nightly:2026.07" 3600 \
          | jq -r .task_id)

# 2. Run the work on a machine we already hold a connect token for.
stronghold_git_clone  "$machine_id" github.com/acme/app.git --branch release
stronghold_exec       "$machine_id" sed 10  -- -i 's/1.2.2/1.2.3/' Cargo.toml
stronghold_git_commit "$machine_id" "bump v1.2.3" -- Cargo.toml
stronghold_git_push   "$machine_id"
pr=$(stronghold_git_pr "$machine_id" "Release v1.2.3" main release \
      | jq -r .pr_url)

# 3. Report back.
stronghold_result "$task_id" 0 "" "" "opened $pr" \
  --artifacts "[{\"pr_url\":\"$pr\"}]"
```

---

## Design notes

- **JSON is built with `jq`**, so arguments with spaces, quotes, or special
  characters are escaped correctly — never hand-rolled.
- **Errors go to stderr; responses go to stdout.** A missing env var or
  unknown flag returns a non-zero exit code with a clear message and emits no
  stdout, so `set -e` pipelines fail loudly.
- **The script is shellcheck-clean.** `SC2016` is disabled file-wide because
  jq filters intentionally use `$var` inside single quotes (the `$` belongs
  to jq, not bash).
- **No network calls happen at source time.** Sourcing only defines
  functions; nothing runs until you call one.

Run `stronghold_help` for a quick reference of every function.
