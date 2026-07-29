#!/usr/bin/env bash
#
# SC2016 is disabled file-wide: jq filters deliberately use `$var` (jq
# parameter syntax) inside single quotes. The `$` belongs to jq, not bash, so
# single-quoting is correct and required — double quotes would make bash
# expand the variables. There is no way to express these filters without a
# literal `$` in a quoted string, so we silence the (false-positive) info.
# shellcheck disable=SC2016
#
# stronghold-agent.sh — Bash SDK for the Stronghold gateway.
#
# Instead of hand-writing curl commands, agents source this file and call
# high-level functions for every gateway endpoint: task lifecycle, structured
# exec, git flow, credential vault, mid-session reprompt, workflow DAGs, and
# the interactive PTY.
#
#   source /path/to/stronghold-agent.sh
#   export STRONGHOLD_URL=https://gateway.example.com
#   export STRONGHOLD_TOKEN=<agent bearer token>
#
#   task_id=$(stronghold_task "build the crate" "stronghold/rust-nightly:2026.07" 3600 \
#             | jq -r .task_id)
#   stronghold_exec "$machine_id" cargo 600 -- build --release
#   stronghold_result "$task_id" 0 "$stdout" "" "build ok"
#
# ─── Authentication model ──────────────────────────────────────────────────
#
# The gateway uses two distinct credentials:
#
#   STRONGHOLD_TOKEN          Agent bearer token (tenant-scoped). Used by the
#                             task, credential, instruct, and workflow
#                             endpoints. Sent as
#                             `Authorization: Bearer <token>`.
#
#   STRONGHOLD_CONNECT_TOKEN  Per-machine connect token returned by
#                             POST /agent/order. Used by the exec, git, and
#                             PTY endpoints. Sent as `?token=<token>` in the
#                             query string. If unset, the SDK falls back to
#                             $STRONGHOLD_TOKEN so a single token works for
#                             everything in dev.
#
# ─── Environment variables ─────────────────────────────────────────────────
#
#   STRONGHOLD_URL            Gateway base URL (required). No trailing slash.
#   STRONGHOLD_TOKEN          Agent bearer token (required for most calls).
#   STRONGHOLD_CONNECT_TOKEN  Per-machine connect token (see above).
#   STRONGHOLD_CURL           Override the curl binary (optional).
#   STRONGHOLD_JQ             Override the jq binary (optional).
#   STRONGHOLD_WEBSOCAT       Override the websocat binary (optional).
#   STRONGHOLD_CURL_FLAGS     Extra flags appended to every curl call
#                             (optional, e.g. "--cacert /etc/stronghold/ca.pem").
#
# ─── Dependencies ──────────────────────────────────────────────────────────
#
#   curl, jq        — required by every function.
#   websocat        — required only by stronghold_shell.
#
# Every public function prints the gateway's JSON response on stdout and
# returns the curl exit code, so callers can pipe to jq and check $?.

# Re-sourcing this file is harmless (it only defines functions), so no guard
# is needed.

# ============================================================================
# Internal helpers (private, prefixed _stronghold_)
# ============================================================================

# Resolve a required binary, honoring an STRONGHOLD_<NAME> override.
# Prints the binary path/name on stdout, or returns 1 with a stderr message.
_stronghold_require() {
  local bin="$1"
  local override=""
  case "$bin" in
    curl)     override="${STRONGHOLD_CURL:-}" ;;
    jq)       override="${STRONGHOLD_JQ:-}" ;;
    websocat) override="${STRONGHOLD_WEBSOCAT:-}" ;;
  esac
  if [ -n "$override" ]; then
    printf '%s' "$override"
    return 0
  fi
  if ! command -v "$bin" >/dev/null 2>&1; then
    printf 'stronghold: required binary %q not found in PATH\n' "$bin" >&2
    return 1
  fi
  printf '%s' "$bin"
}

# Print the gateway base URL (trailing slashes stripped) or fail.
_stronghold_url() {
  if [ -z "${STRONGHOLD_URL:-}" ]; then
    echo "stronghold: STRONGHOLD_URL is not set" >&2
    return 1
  fi
  printf '%s' "${STRONGHOLD_URL%/}"
}

# Print the connect token (STRONGHOLD_CONNECT_TOKEN or fallback to
# STRONGHOLD_TOKEN), or fail.
_stronghold_connect_token() {
  local tok="${STRONGHOLD_CONNECT_TOKEN:-${STRONGHOLD_TOKEN:-}}"
  if [ -z "$tok" ]; then
    echo "stronghold: STRONGHOLD_CONNECT_TOKEN (or STRONGHOLD_TOKEN) is not set" >&2
    return 1
  fi
  printf '%s' "$tok"
}

# Build a full URL for a connect-token endpoint: <base><path>?token=<encoded>.
# URL-encodes the token so special characters are safe.
_stronghold_token_url() {
  local path="$1"
  local base jq tok enc
  base="$(_stronghold_url)" || return 1
  tok="$(_stronghold_connect_token)" || return 1
  jq="$(_stronghold_require jq)" || return 1
  enc="$("$jq" -rn --arg v "$tok" '$v | @uri')" || return 1
  printf '%s%s?token=%s' "$base" "$path" "$enc"
}

# curl wrapper for bearer-token endpoints (task, credential, instruct,
# workflow). Caller supplies method, URL, and any -d / -G flags.
_stronghold_curl() {
  if [ -z "${STRONGHOLD_TOKEN:-}" ]; then
    echo "stronghold: STRONGHOLD_TOKEN is not set" >&2
    return 1
  fi
  local curl
  curl="$(_stronghold_require curl)" || return 1
  # shellcheck disable=SC2086 # STRONGHOLD_CURL_FLAGS is intentionally unquoted
  "$curl" -sS \
    -H "Authorization: Bearer ${STRONGHOLD_TOKEN}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    $STRONGHOLD_CURL_FLAGS \
    "$@"
}

# curl wrapper for connect-token endpoints (exec, git). The token is already
# embedded in the URL, so no Authorization header is sent.
_stronghold_token_curl() {
  local curl
  curl="$(_stronghold_require curl)" || return 1
  # shellcheck disable=SC2086 # STRONGHOLD_CURL_FLAGS is intentionally unquoted
  "$curl" -sS \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    $STRONGHOLD_CURL_FLAGS \
    "$@"
}

# Build a JSON object from KEY=VALUE pairs, returning it on stdout. Values are
# treated as strings. Used for env maps.
_stronghold_kv_to_json() {
  local jq out k v
  jq="$(_stronghold_require jq)" || return 1
  out='{}'
  for pair in "$@"; do
    k="${pair%%=*}"
    v="${pair#*=}"
    out="$(printf '%s' "$out" | "$jq" --arg k "$k" --arg v "$v" '. + {($k): $v}')"
  done
  printf '%s' "$out"
}

# Build a JSON array from positional string arguments.
# The `--` after `--args` forces every remaining argument to be treated as a
# positional string (so values like `--release` or `--jsonargs` are preserved
# rather than parsed as jq options).
_stronghold_array_to_json() {
  local jq
  jq="$(_stronghold_require jq)" || return 1
  if [ "$#" -gt 0 ]; then
    "$jq" -n '$ARGS.positional' --args -- "$@"
  else
    printf '[]'
  fi
}

# ============================================================================
# Task lifecycle
# ============================================================================

# stronghold_task INSTRUCTION IMAGE TTL_SECS
#   [--context JSON] [--parent TASK_ID] [--workflow-run RUN_ID]
#
# POST /agent/task — create a new queued task.
stronghold_task() {
  if [ "$#" -lt 3 ]; then
    echo "usage: stronghold_task INSTRUCTION IMAGE TTL_SECS [--context JSON] [--parent TASK_ID] [--workflow-run RUN_ID]" >&2
    return 2
  fi
  local instruction="$1" image="$2" ttl="$3"
  shift 3
  local context="" parent="" wrun=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --context)      context="$2"; shift 2 ;;
      --parent)       parent="$2";  shift 2 ;;
      --workflow-run) wrun="$2";    shift 2 ;;
      -h|--help)      echo "usage: stronghold_task INSTRUCTION IMAGE TTL_SECS [--context JSON] [--parent TASK_ID] [--workflow-run RUN_ID]"; return 0 ;;
      *) echo "stronghold_task: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq base json
  jq="$(_stronghold_require jq)" || return 1
  base="$(_stronghold_url)" || return 1
  json="$("$jq" -n \
    --arg instruction "$instruction" \
    --arg image "$image" \
    --argjson ttl "$ttl" \
    '{instruction: $instruction, image: $image, ttl_secs: $ttl}')" || return 1
  if [ -n "$context" ]; then
    json="$(printf '%s' "$json" | "$jq" --argjson c "$context" '. + {context: $c}')" || return 1
  fi
  if [ -n "$parent" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$parent" '. + {parent_task_id: $v}')" || return 1
  fi
  if [ -n "$wrun" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$wrun" '. + {workflow_run_id: $v}')" || return 1
  fi
  _stronghold_curl -X POST "$base/agent/task" -d "$json"
}

# stronghold_result TASK_ID EXIT_CODE STDOUT STDERR SUMMARY [--artifacts JSON_ARRAY]
#
# POST /agent/task/:id/result — submit a task's execution result.
# exit_code 0 marks the task completed; non-zero marks it failed.
stronghold_result() {
  if [ "$#" -lt 5 ]; then
    echo "usage: stronghold_result TASK_ID EXIT_CODE STDOUT STDERR SUMMARY [--artifacts JSON_ARRAY]" >&2
    return 2
  fi
  local task_id="$1" exit_code="$2" stdout="$3" stderr="$4" summary="$5"
  shift 5
  local artifacts=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --artifacts) artifacts="$2"; shift 2 ;;
      -h|--help)   echo "usage: stronghold_result TASK_ID EXIT_CODE STDOUT STDERR SUMMARY [--artifacts JSON_ARRAY]"; return 0 ;;
      *) echo "stronghold_result: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq base json
  jq="$(_stronghold_require jq)" || return 1
  base="$(_stronghold_url)" || return 1
  json="$("$jq" -n \
    --argjson exit_code "$exit_code" \
    --arg stdout "$stdout" \
    --arg stderr "$stderr" \
    --arg summary "$summary" \
    '{exit_code: $exit_code, stdout: $stdout, stderr: $stderr, summary: $summary, artifacts: []}')" || return 1
  if [ -n "$artifacts" ]; then
    json="$(printf '%s' "$json" | "$jq" --argjson a "$artifacts" '.artifacts = $a')" || return 1
  fi
  _stronghold_curl -X POST "$base/agent/task/${task_id}/result" -d "$json"
}

# ============================================================================
# Structured command execution
# ============================================================================

# stronghold_exec MACHINE_ID CMD TIMEOUT_SECS
#   [--cwd DIR] [--env KEY=VAL ...] [-- ARG ...]
#
# POST /agent/:machine_id/exec — run a non-interactive command and return
# structured {exit_code, stdout, stderr, duration_ms, audit_seq} JSON.
# Arguments after `--` are passed as the command's argv. `--env` may repeat.
stronghold_exec() {
  if [ "$#" -lt 3 ]; then
    echo "usage: stronghold_exec MACHINE_ID CMD TIMEOUT_SECS [--cwd DIR] [--env KEY=VAL ...] [-- ARG ...]" >&2
    return 2
  fi
  local machine_id="$1" cmd="$2" timeout="$3"
  shift 3
  local cwd=""
  local -a env_pairs=() args=()
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --cwd) cwd="$2"; shift 2 ;;
      --env) env_pairs+=("$2"); shift 2 ;;
      --)    shift; args=("$@"); break ;;
      -h|--help) echo "usage: stronghold_exec MACHINE_ID CMD TIMEOUT_SECS [--cwd DIR] [--env KEY=VAL ...] [-- ARG ...]"; return 0 ;;
      *) echo "stronghold_exec: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq url json args_json env_json
  jq="$(_stronghold_require jq)" || return 1
  url="$(_stronghold_token_url "/agent/${machine_id}/exec")" || return 1
  json="$("$jq" -n \
    --arg cmd "$cmd" \
    --argjson timeout "$timeout" \
    '{cmd: $cmd, args: [], timeout_secs: $timeout, env: {}}')" || return 1
  args_json="$(_stronghold_array_to_json "${args[@]}")" || return 1
  json="$(printf '%s' "$json" | "$jq" --argjson a "$args_json" '.args = $a')" || return 1
  if [ -n "$cwd" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$cwd" '.cwd = $v')" || return 1
  fi
  if [ "${#env_pairs[@]}" -gt 0 ]; then
    env_json="$(_stronghold_kv_to_json "${env_pairs[@]}")" || return 1
    json="$(printf '%s' "$json" | "$jq" --argjson e "$env_json" '.env = $e')" || return 1
  fi
  _stronghold_token_curl -X POST "$url" -d "$json"
}

# ============================================================================
# Git flow
# ============================================================================

# stronghold_git_clone MACHINE_ID REPO [--branch NAME] [--path DIR]
#
# POST /agent/:machine_id/git/clone — clone a repo into the workspace pod.
# The tenant's stored git credential is injected server-side; the token never
# crosses this API.
stronghold_git_clone() {
  if [ "$#" -lt 2 ]; then
    echo "usage: stronghold_git_clone MACHINE_ID REPO [--branch NAME] [--path DIR]" >&2
    return 2
  fi
  local machine_id="$1" repo="$2"
  shift 2
  local branch="" path=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --branch) branch="$2"; shift 2 ;;
      --path)   path="$2";   shift 2 ;;
      -h|--help) echo "usage: stronghold_git_clone MACHINE_ID REPO [--branch NAME] [--path DIR]"; return 0 ;;
      *) echo "stronghold_git_clone: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq url json
  jq="$(_stronghold_require jq)" || return 1
  url="$(_stronghold_token_url "/agent/${machine_id}/git/clone")" || return 1
  json="$("$jq" -n --arg repo "$repo" '{repo: $repo}')" || return 1
  if [ -n "$branch" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$branch" '.branch = $v')" || return 1
  fi
  if [ -n "$path" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$path" '.path = $v')" || return 1
  fi
  _stronghold_token_curl -X POST "$url" -d "$json"
}

# stronghold_git_branch MACHINE_ID NAME [--from REF] [--path DIR]
#
# POST /agent/:machine_id/git/branch — create + check out a new branch.
stronghold_git_branch() {
  if [ "$#" -lt 2 ]; then
    echo "usage: stronghold_git_branch MACHINE_ID NAME [--from REF] [--path DIR]" >&2
    return 2
  fi
  local machine_id="$1" name="$2"
  shift 2
  local from="" path=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --from) from="$2"; shift 2 ;;
      --path) path="$2"; shift 2 ;;
      -h|--help) echo "usage: stronghold_git_branch MACHINE_ID NAME [--from REF] [--path DIR]"; return 0 ;;
      *) echo "stronghold_git_branch: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq url json
  jq="$(_stronghold_require jq)" || return 1
  url="$(_stronghold_token_url "/agent/${machine_id}/git/branch")" || return 1
  json="$("$jq" -n --arg name "$name" '{name: $name}')" || return 1
  if [ -n "$from" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$from" '.from = $v')" || return 1
  fi
  if [ -n "$path" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$path" '.path = $v')" || return 1
  fi
  _stronghold_token_curl -X POST "$url" -d "$json"
}

# stronghold_git_commit MACHINE_ID MESSAGE [-- FILE ...] [--path DIR]
#
# POST /agent/:machine_id/git/commit — stage files and commit. With no `--`
# files, stages everything (`git add -A`).
stronghold_git_commit() {
  if [ "$#" -lt 2 ]; then
    echo "usage: stronghold_git_commit MACHINE_ID MESSAGE [-- FILE ...] [--path DIR]" >&2
    return 2
  fi
  local machine_id="$1" message="$2"
  shift 2
  local -a files=()
  local path=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --path) path="$2"; shift 2 ;;
      --) shift; files=("$@"); break ;;
      -h|--help) echo "usage: stronghold_git_commit MACHINE_ID MESSAGE [-- FILE ...] [--path DIR]"; return 0 ;;
      *) echo "stronghold_git_commit: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq url json files_json
  jq="$(_stronghold_require jq)" || return 1
  url="$(_stronghold_token_url "/agent/${machine_id}/git/commit")" || return 1
  json="$("$jq" -n --arg message "$message" '{message: $message}')" || return 1
  if [ "${#files[@]}" -gt 0 ]; then
    files_json="$(_stronghold_array_to_json "${files[@]}")" || return 1
    json="$(printf '%s' "$json" | "$jq" --argjson f "$files_json" '.files = $f')" || return 1
  fi
  if [ -n "$path" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$path" '.path = $v')" || return 1
  fi
  _stronghold_token_curl -X POST "$url" -d "$json"
}

# stronghold_git_push MACHINE_ID [--remote NAME] [--branch NAME] [--path DIR]
#
# POST /agent/:machine_id/git/push — push to a remote (default origin / HEAD).
stronghold_git_push() {
  if [ "$#" -lt 1 ]; then
    echo "usage: stronghold_git_push MACHINE_ID [--remote NAME] [--branch NAME] [--path DIR]" >&2
    return 2
  fi
  local machine_id="$1"
  shift
  local remote="" branch="" path=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --remote) remote="$2"; shift 2 ;;
      --branch) branch="$2"; shift 2 ;;
      --path)   path="$2";   shift 2 ;;
      -h|--help) echo "usage: stronghold_git_push MACHINE_ID [--remote NAME] [--branch NAME] [--path DIR]"; return 0 ;;
      *) echo "stronghold_git_push: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq url json
  jq="$(_stronghold_require jq)" || return 1
  url="$(_stronghold_token_url "/agent/${machine_id}/git/push")" || return 1
  json="$("$jq" -n '{}')" || return 1
  if [ -n "$remote" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$remote" '.remote = $v')" || return 1
  fi
  if [ -n "$branch" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$branch" '.branch = $v')" || return 1
  fi
  if [ -n "$path" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$path" '.path = $v')" || return 1
  fi
  _stronghold_token_curl -X POST "$url" -d "$json"
}

# stronghold_git_pr MACHINE_ID TITLE BASE HEAD [--body TEXT]
#
# POST /agent/:machine_id/git/pr — open a GitHub pull request. Requires a
# stored `github-pat` credential; the handler resolves owner/repo from the
# pod's `origin` remote.
stronghold_git_pr() {
  if [ "$#" -lt 4 ]; then
    echo "usage: stronghold_git_pr MACHINE_ID TITLE BASE HEAD [--body TEXT]" >&2
    return 2
  fi
  local machine_id="$1" title="$2" base="$3" head="$4"
  shift 4
  local body=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --body) body="$2"; shift 2 ;;
      -h|--help) echo "usage: stronghold_git_pr MACHINE_ID TITLE BASE HEAD [--body TEXT]"; return 0 ;;
      *) echo "stronghold_git_pr: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq url json
  jq="$(_stronghold_require jq)" || return 1
  url="$(_stronghold_token_url "/agent/${machine_id}/git/pr")" || return 1
  json="$("$jq" -n \
    --arg title "$title" \
    --arg base "$base" \
    --arg head "$head" \
    '{title: $title, base: $base, head: $head}')" || return 1
  if [ -n "$body" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$body" '.body = $v')" || return 1
  fi
  _stronghold_token_curl -X POST "$url" -d "$json"
}

# ============================================================================
# Credentials
# ============================================================================

# stronghold_credential MACHINE_ID NAME
#
# GET /agent/:machine_id/credentials/:name — fetch + decrypt a named
# credential for the machine's tenant. The value is returned in the response
# body; it is never logged by the gateway.
stronghold_credential() {
  if [ "$#" -lt 2 ]; then
    echo "usage: stronghold_credential MACHINE_ID NAME" >&2
    return 2
  fi
  local machine_id="$1" name="$2"
  local base
  base="$(_stronghold_url)" || return 1
  _stronghold_curl -X GET "$base/agent/${machine_id}/credentials/${name}"
}

# ============================================================================
# Mid-session reprompt
# ============================================================================

# stronghold_instruct MACHINE_ID INSTRUCTION
#   [--mode pty|control|task] [--priority low|normal|high] [--context JSON]
#
# POST /agent/:machine_id/instruct — inject a new instruction into a running
# session. `pty` (default) types into the live PTY; `task` queues a sub-task;
# `control` is reserved for the N4 control channel.
stronghold_instruct() {
  if [ "$#" -lt 2 ]; then
    echo "usage: stronghold_instruct MACHINE_ID INSTRUCTION [--mode pty|control|task] [--priority low|normal|high] [--context JSON]" >&2
    return 2
  fi
  local machine_id="$1" instruction="$2"
  shift 2
  local mode="" priority="" context=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --mode)     mode="$2";     shift 2 ;;
      --priority) priority="$2"; shift 2 ;;
      --context)  context="$2";  shift 2 ;;
      -h|--help)  echo "usage: stronghold_instruct MACHINE_ID INSTRUCTION [--mode pty|control|task] [--priority low|normal|high] [--context JSON]"; return 0 ;;
      *) echo "stronghold_instruct: unknown option: $1" >&2; return 2 ;;
    esac
  done
  local jq base json
  jq="$(_stronghold_require jq)" || return 1
  base="$(_stronghold_url)" || return 1
  json="$("$jq" -n --arg instruction "$instruction" '{instruction: $instruction}')" || return 1
  if [ -n "$mode" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$mode" '.mode = $v')" || return 1
  fi
  if [ -n "$priority" ]; then
    json="$(printf '%s' "$json" | "$jq" --arg v "$priority" '.priority = $v')" || return 1
  fi
  if [ -n "$context" ]; then
    json="$(printf '%s' "$json" | "$jq" --argjson c "$context" '.context = $c')" || return 1
  fi
  _stronghold_curl -X POST "$base/agent/${machine_id}/instruct" -d "$json"
}

# ============================================================================
# Interactive PTY
# ============================================================================

# stronghold_shell MACHINE_ID [WEBSOCAT_FLAGS ...]
#
# Opens an interactive PTY over the gateway's WebSocket endpoint
# (wss://<host>/agent/<machine_id>/pty?token=<connect_token>) using websocat.
# Extra arguments are forwarded to websocat (e.g. -b for binary, -t for text).
# The connect token is URL-encoded and appended automatically.
stronghold_shell() {
  if [ "$#" -lt 1 ]; then
    echo "usage: stronghold_shell MACHINE_ID [WEBSOCAT_FLAGS ...]" >&2
    return 2
  fi
  local machine_id="$1"
  shift
  local ws base jq tok enc wsurl
  ws="$(_stronghold_require websocat)" || return 1
  base="$(_stronghold_url)" || return 1
  tok="$(_stronghold_connect_token)" || return 1
  jq="$(_stronghold_require jq)" || return 1
  enc="$("$jq" -rn --arg v "$tok" '$v | @uri')" || return 1
  case "$base" in
    https://*) wsurl="wss://${base#https://}/agent/${machine_id}/pty?token=${enc}" ;;
    http://*)  wsurl="ws://${base#http://}/agent/${machine_id}/pty?token=${enc}" ;;
    *)         wsurl="wss://${base}/agent/${machine_id}/pty?token=${enc}" ;;
  esac
  "$ws" -t "$@" "$wsurl"
}

# ============================================================================
# Workflows
# ============================================================================

# stronghold_workflow_create NAME DAG_JSON
#
# POST /workflow — define a new workflow (status = draft). DAG_JSON is a JSON
# object of the form `{"steps": [...]}` stored verbatim and parsed at run time.
stronghold_workflow_create() {
  if [ "$#" -lt 2 ]; then
    echo "usage: stronghold_workflow_create NAME DAG_JSON" >&2
    return 2
  fi
  local name="$1" dag="$2"
  local jq base json
  jq="$(_stronghold_require jq)" || return 1
  base="$(_stronghold_url)" || return 1
  json="$("$jq" -n --arg name "$name" --argjson dag "$dag" '{name: $name, dag: $dag}')" || return 1
  _stronghold_curl -X POST "$base/workflow" -d "$json"
}

# stronghold_workflow_run WORKFLOW_ID
#
# POST /workflow/:id/run — start a background run of a defined workflow.
# Returns immediately with {run_id, status}; poll with stronghold_workflow_status.
stronghold_workflow_run() {
  if [ "$#" -lt 1 ]; then
    echo "usage: stronghold_workflow_run WORKFLOW_ID" >&2
    return 2
  fi
  local workflow_id="$1"
  local base
  base="$(_stronghold_url)" || return 1
  # No request body is required by the handler; send {} for a clean POST.
  _stronghold_curl -X POST "$base/workflow/${workflow_id}/run" -d '{}'
}

# stronghold_workflow_status RUN_ID
#
# GET /workflow/run/:id — poll a workflow run's status and step progress.
stronghold_workflow_status() {
  if [ "$#" -lt 1 ]; then
    echo "usage: stronghold_workflow_status RUN_ID" >&2
    return 2
  fi
  local run_id="$1"
  local base
  base="$(_stronghold_url)" || return 1
  _stronghold_curl -X GET "$base/workflow/run/${run_id}"
}

# ============================================================================
# Help
# ============================================================================

# stronghold_help — list every public function with a one-line summary.
stronghold_help() {
  cat <<'EOF'
Stronghold agent bash SDK — available functions:

  stronghold_task INSTRUCTION IMAGE TTL [--context JSON] [--parent ID] [--workflow-run ID]
      Create a queued task (POST /agent/task).

  stronghold_result TASK_ID EXIT_CODE STDOUT STDERR SUMMARY [--artifacts JSON]
      Submit a task's result (POST /agent/task/:id/result).

  stronghold_exec MACHINE CMD TTL [--cwd DIR] [--env K=V] [-- ARG...]
      Run a command, return structured JSON (POST /agent/:m/exec).

  stronghold_git_clone MACHINE REPO [--branch NAME] [--path DIR]
      Clone a repo (POST /agent/:m/git/clone).

  stronghold_git_branch MACHINE NAME [--from REF]
      Create + checkout a branch (POST /agent/:m/git/branch).

  stronghold_git_commit MACHINE MESSAGE [-- FILE...]
      Stage + commit (POST /agent/:m/git/commit).

  stronghold_git_push MACHINE [--remote NAME] [--branch NAME]
      Push to a remote (POST /agent/:m/git/push).

  stronghold_git_pr MACHINE TITLE BASE HEAD [--body TEXT]
      Open a GitHub PR (POST /agent/:m/git/pr).

  stronghold_credential MACHINE NAME
      Fetch + decrypt a named credential (GET /agent/:m/credentials/:name).

  stronghold_instruct MACHINE INSTRUCTION [--mode pty|control|task] [--priority ...] [--context JSON]
      Mid-session reprompt (POST /agent/:m/instruct).

  stronghold_shell MACHINE [WEBSOCAT_FLAGS...]
      Open an interactive PTY over WebSocket (websocat).

  stronghold_workflow_create NAME DAG_JSON
      Define a workflow (POST /workflow).

  stronghold_workflow_run WORKFLOW_ID
      Start a workflow run (POST /workflow/:id/run).

  stronghold_workflow_status RUN_ID
      Poll a run (GET /workflow/run/:id).

Env: STRONGHOLD_URL, STRONGHOLD_TOKEN, STRONGHOLD_CONNECT_TOKEN,
     STRONGHOLD_CURL_FLAGS, STRONGHOLD_CURL, STRONGHOLD_JQ, STRONGHOLD_WEBSOCAT.
EOF
}
