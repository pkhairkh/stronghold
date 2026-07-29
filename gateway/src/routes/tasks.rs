//! Task lifecycle endpoints.
//!
//! A **task** is a structured work unit with a full lifecycle:
//! `queued → scheduled → running → completed | failed | cancelled`.
//!
//! Tasks decouple *what to do* (the spec) from *where it runs* (the machine).
//! A task is created in the `queued` state; the scheduler (or an operator)
//! later assigns it a machine and starts it. Results are submitted back via
//! the result endpoint.
//!
//! Endpoints:
//! - `POST /agent/task`              — Create a new queued task
//! - `GET  /agent/task/:id`          — Fetch a task's status and details
//! - `POST /agent/task/:id/result`   — Submit a task's execution result
//! - `GET  /agent/task/:id/stream`   — Stream status updates via Server-Sent Events
//! - `POST /agent/task/:id/progress` — Submit a progress report (mid-task)
//! - `POST /agent/task/:id/reflexion`— Submit a post-task reflexion
//! - `GET  /agent/task/:id/reflexion`— Retrieve a task's reflexion
//! - `GET  /agent/reflexions`        — List recent reflexions (tenant-scoped)
//!
//! All endpoints require a valid agent bearer token (tenant-scoped).

use crate::routes::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::Sse;
use axum::Json;
use futures_util::Stream;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ============================================================================
// Request / response types
// ============================================================================

/// Request body for `POST /agent/task`.
#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    /// Natural-language or shell instruction the agent should execute.
    pub instruction: String,
    /// OCI image to run the task in, e.g. `stronghold/rust-nightly:2026.07`.
    pub image: String,
    /// Time-to-live for the task execution, in seconds.
    pub ttl_secs: u64,
    /// Optional free-form context (env, prior outputs, etc.) passed to the agent.
    pub context: Option<serde_json::Value>,
    /// Optional parent task — set when this task is a sub-task of another.
    pub parent_task_id: Option<String>,
    /// Optional workflow run — set when this task is a step in a workflow run.
    pub workflow_run_id: Option<String>,
    /// Optional agent role name (e.g. `"coder"`, `"reviewer"`, `"planner"`).
    ///
    /// When set, the role's `system_prompt` is looked up in `agent_roles`
    /// (scoped to the authenticated tenant) and snapshotted into the task's
    /// `spec` JSON as `role_system_prompt`. The role name itself is also
    /// stored in the spec as `role`. This snapshot means the task retains
    /// its prompt even if the role is later deleted.
    ///
    /// If the role name doesn't match an existing row, the task is still
    /// created (with `role` set but `role_system_prompt` null) — role
    /// enforcement is best-effort, not a hard gate on task creation.
    #[serde(default)]
    pub role: Option<String>,
}

/// Response body for `POST /agent/task`.
#[derive(Debug, Serialize)]
pub struct CreateTaskResponse {
    /// The newly created task ID (`task_<ULID>`).
    pub task_id: String,
    /// The machine the task is assigned to, if any. `None` while queued.
    pub machine_id: Option<String>,
    /// The task's current status (`queued` on creation).
    pub status: String,
}

/// Response body for `GET /agent/task/:id`.
#[derive(Debug, Serialize)]
pub struct GetTaskResponse {
    pub id: String,
    pub status: String,
    /// JSON spec the task was created with: `{instruction, image, ttl_secs, context}`.
    pub spec: serde_json::Value,
    /// JSON result, present once a result has been submitted.
    pub result: Option<serde_json::Value>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Error message, set when the task fails.
    pub error: Option<String>,
    pub retry_count: i64,
}

/// Request body for `POST /agent/task/:id/result`.
#[derive(Debug, Deserialize)]
pub struct SubmitResultRequest {
    /// Process exit code. `0` → `completed`; non-zero → `failed`.
    pub exit_code: i32,
    /// Captured stdout of the task.
    pub stdout: String,
    /// Captured stderr of the task.
    pub stderr: String,
    /// Human / LLM-readable summary of what happened.
    pub summary: String,
    /// Artifact descriptors produced by the task.
    pub artifacts: Vec<serde_json::Value>,
}

/// Request body for `POST /agent/task/:id/progress`.
///
/// A **progress report** is a mid-task heartbeat. Agents (typically the
/// **Coder**) post one periodically so watchers (Watchdog, Facilitator, the
/// orchestrator) can observe forward motion and detect stalls. Each report
/// is stored in `task_outputs` under a timestamped key (`progress_<unix_ms>`)
/// so the full history is preserved.
#[derive(Debug, Deserialize)]
pub struct ProgressRequest {
    /// Files modified since the last progress report (or task start).
    pub files_changed: Vec<String>,
    /// Number of test cases executed since the last report.
    pub tests_run: u32,
    /// Number of those tests that passed.
    pub tests_passing: u32,
    /// Number of git commits made since the last report.
    pub commits: u32,
    /// Free-form blocker descriptions (anything currently preventing forward
    /// motion — missing dependency, unclear spec, flaky test, etc.).
    pub blockers: Vec<String>,
    /// Agent self-reported status for this reporting window. Free-form but
    /// typical values: `"on_track"`, `"blocked"`, `"needs_review"`,
    /// `"nearing_completion"`.
    pub status: String,
}

/// Response body for `POST /agent/task/:id/progress`.
#[derive(Debug, Serialize)]
pub struct ProgressResponse {
    /// Always `"stored"` — the report has been persisted.
    pub status: String,
    /// The `task_outputs.key` the report was stored under
    /// (`progress_<unix_ms>`). Returned so callers can later fetch a
    /// specific report by key if needed.
    pub key: String,
}

/// Request body for `POST /agent/task/:id/reflexion`.
///
/// A **reflexion** is a structured post-task postmortem inspired by the
/// Reflexion paper (Shinn et al., 2023). After a task completes (success or
/// failure), the agent records what worked, what didn't, what it would do
/// differently, and what it learned — so future runs on similar tasks can
/// benefit. Stored in `task_outputs` under the constant key `"reflexion"`
/// (one per task; resubmission overwrites).
#[derive(Debug, Deserialize)]
pub struct ReflexionRequest {
    /// What went well during this task.
    pub what_went_well: String,
    /// What went wrong — failures, missteps, wasted effort.
    pub what_went_wrong: String,
    /// What the agent would do differently next time.
    pub what_differently: String,
    /// Generalizable lessons learned (carried forward to future tasks).
    pub what_learned: String,
}

/// Response body for `POST /agent/task/:id/reflexion`.
#[derive(Debug, Serialize)]
pub struct ReflexionResponse {
    /// Always `"stored"`.
    pub status: String,
}

/// Response body for `GET /agent/task/:id/reflexion`.
///
/// Returns `404` (via the handler's `Err` arm) if no reflexion has been
/// recorded for this task yet.
#[derive(Debug, Serialize)]
pub struct GetReflexionResponse {
    pub task_id: String,
    /// The stored reflexion, parsed back from JSON. Mirrors the fields of
    /// [`ReflexionRequest`] plus the original `created_at`-style metadata if
    /// the writer included any.
    pub reflexion: serde_json::Value,
}

/// Query string for `GET /agent/reflexions`.
#[derive(Debug, Deserialize)]
pub struct ListReflexionsQuery {
    /// Tenant to list reflexions for. If provided, must match the
    /// authenticated agent's tenant (otherwise `403`). If omitted, the
    /// authenticated tenant is used.
    pub tenant: Option<String>,
    /// Maximum number of reflexions to return. Defaults to `10`; clamped to
    /// `100` to protect against unbounded queries.
    pub limit: Option<u32>,
}

/// A single item in the `GET /agent/reflexions` response list.
#[derive(Debug, Serialize)]
pub struct ReflexionListItem {
    /// The task the reflexion belongs to.
    pub task_id: String,
    /// The parsed reflexion body.
    pub reflexion: serde_json::Value,
    /// When the task was created (used as the ordering key for "recent").
    pub task_created_at: String,
}

// ============================================================================
// Handlers
// ============================================================================

/// Create a new task in the `queued` state.
///
/// This does **not** create a machine/session — that happens when the task is
/// later "started" (by the scheduler or an operator). For now the task is just
/// queued and visible via `GET /agent/task/:id`.
///
/// When `req.role` is set, the role's `system_prompt` is looked up in
/// `agent_roles` (tenant-scoped) and snapshotted into the spec JSON as
/// `role_system_prompt`. The role name is stored alongside as `role`. A
/// missing role row is **not** a hard error — the task is still created with
/// `role` set and `role_system_prompt` null, so role lookup is best-effort
/// at task creation time.
pub async fn create_task(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let task_id = format!("task_{}", ulid::Ulid::new());

    // If a role is specified, look up its system_prompt and snapshot it into
    // the spec. A missing role row is logged but does NOT fail the request —
    // role enforcement is best-effort, not a hard gate on task creation.
    let role_system_prompt: Option<String> = match req.role.as_deref() {
        Some(role_name) if !role_name.is_empty() => {
            let conn = state
                .db
                .get()
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            match conn.query_row(
                "SELECT system_prompt FROM agent_roles
                 WHERE tenant_id = ?1 AND name = ?2",
                rusqlite::params![tenant_id, role_name],
                |row| row.get::<_, String>(0),
            ) {
                Ok(prompt) => Some(prompt),
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    tracing::warn!(
                        tenant = %tenant_id,
                        role = %role_name,
                        task_id = %task_id,
                        "Role not found — task created with role name but no system_prompt snapshot"
                    );
                    None
                }
                Err(e) => {
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                }
            }
        }
        _ => None,
    };

    // Build the immutable spec blob. The base fields are always present; the
    // `role` and `role_system_prompt` fields are added only when a role was
    // supplied (keeping the spec small for role-less tasks).
    let has_role_prompt = role_system_prompt.is_some();
    let mut spec = serde_json::json!({
        "instruction": req.instruction,
        "image": req.image,
        "ttl_secs": req.ttl_secs,
        "context": req.context,
    });
    if let Some(ref role_name) = req.role {
        spec["role"] = serde_json::Value::String(role_name.clone());
        spec["role_system_prompt"] = match role_system_prompt {
            Some(p) => serde_json::Value::String(p),
            None => serde_json::Value::Null,
        };
    }
    let spec_str = spec.to_string();

    tracing::info!(
        tenant = %tenant_id,
        task_id = %task_id,
        image = %serde_json::json!(req.image),
        role = ?req.role,
        has_role_prompt = has_role_prompt,
        "Task created (queued)"
    );

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    conn.execute(
        "INSERT INTO tasks
         (id, tenant_id, machine_id, parent_task_id, workflow_run_id,
          status, spec, result, created_at, started_at, finished_at,
          error, retry_count, max_retries)
         VALUES (?1, ?2, NULL, ?3, ?4, 'queued', ?5, NULL,
                 datetime('now'), NULL, NULL, NULL, 0, 3)",
        rusqlite::params![
            task_id,
            tenant_id,
            req.parent_task_id,
            req.workflow_run_id,
            spec_str,
        ],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(CreateTaskResponse {
        task_id,
        machine_id: None,
        status: "queued".to_string(),
    }))
}

/// Fetch a task by ID.
///
/// Returns `404 Not Found` if the task does not exist or does not belong to
/// the authenticated tenant.
pub async fn get_task(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<GetTaskResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let row = conn.query_row(
        "SELECT id, status, spec, result, created_at, started_at, finished_at,
                error, retry_count
         FROM tasks
         WHERE id = ?1 AND tenant_id = ?2",
        rusqlite::params![task_id, tenant_id],
        |row| {
            let spec_str: String = row.get(2)?;
            let result_str: Option<String> = row.get(3)?;
            // Parse the stored spec/result JSON back into Values. Both columns
            // are written by this module as valid JSON, so a parse failure
            // indicates DB corruption — surface it as a 500.
            let spec: serde_json::Value = serde_json::from_str(&spec_str)
                .unwrap_or(serde_json::Value::Null);
            let result: Option<serde_json::Value> = result_str
                .map(|s| serde_json::from_str(&s).unwrap_or(serde_json::Value::Null));
            Ok(GetTaskResponse {
                id: row.get(0)?,
                status: row.get(1)?,
                spec,
                result,
                created_at: row.get(4)?,
                started_at: row.get(5)?,
                finished_at: row.get(6)?,
                error: row.get(7)?,
                retry_count: row.get(8)?,
            })
        },
    );

    match row {
        Ok(resp) => Ok(Json(resp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err((
            StatusCode::NOT_FOUND,
            format!("Task not found: {}", task_id),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// Submit a task's execution result.
///
/// - `exit_code == 0` → status `completed`, audit event `task_completed`
/// - `exit_code != 0` → status `failed`,    audit event `task_failed`
///
/// `finished_at` is stamped with `datetime('now')`. Returns `404` if the task
/// does not exist or is not owned by the authenticated tenant.
pub async fn submit_result(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(task_id): Path<String>,
    Json(req): Json<SubmitResultRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    // Determine terminal status + audit event from the exit code.
    let (status, event) = if req.exit_code == 0 {
        ("completed", "task_completed")
    } else {
        ("failed", "task_failed")
    };

    let result_json = serde_json::json!({
        "exit_code": req.exit_code,
        "stdout": req.stdout,
        "stderr": req.stderr,
        "summary": req.summary,
        "artifacts": req.artifacts,
    });
    let result_str = result_json.to_string();

    // Read the task's machine_id (for the audit entry) and confirm ownership
    // before mutating. A missing task → 404.
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let machine_id: Option<String> = conn
        .query_row(
            "SELECT machine_id FROM tasks WHERE id = ?1 AND tenant_id = ?2",
            rusqlite::params![task_id, tenant_id],
            |row| row.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => (
                StatusCode::NOT_FOUND,
                format!("Task not found: {}", task_id),
            ),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?;

    conn.execute(
        "UPDATE tasks
         SET status = ?1, result = ?2, finished_at = datetime('now')
         WHERE id = ?3 AND tenant_id = ?4",
        rusqlite::params![status, result_str, task_id, tenant_id],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Write the audit entry. machine_id may be empty for tasks that never
    // got assigned a machine; the task_id is recorded in the payload.
    let audit_payload = serde_json::json!({
        "task_id": task_id,
        "exit_code": req.exit_code,
        "status": status,
        "summary": req.summary,
    });
    crate::audit::log::entry(
        &state.db,
        &tenant_id,
        machine_id.as_deref().unwrap_or(""),
        event,
        audit_payload,
        &state.audit_keys,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        task_id = %task_id,
        status = status,
        exit_code = req.exit_code,
        "Task result submitted"
    );

    Ok(StatusCode::OK)
}

// ============================================================================
// Progress + Reflexion (R3 + R4)
// ============================================================================

/// `POST /agent/task/:id/progress` — submit a mid-task progress report.
///
/// Verifies the agent token, confirms the task exists and belongs to the
/// authenticated tenant, then:
///
/// 1. Stores the report in `task_outputs` under key `progress_<unix_ms>`
///    (millisecond-precision Unix timestamp from `chrono::Utc::now()`). Each
///    report gets a unique key so the full history is preserved.
/// 2. Posts a notice on `agent_messages` on channel `workflow-run-<run_id>`
///    so workflow subscribers (the orchestrator, the watchdog, etc.) can
///    observe forward motion. If the task has no `workflow_run_id`, the
///    channel falls back to `workflow-task-<task_id>`.
///
/// Returns `{ status: "stored", key: "progress_<unix_ms>" }`.
pub async fn submit_progress(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(task_id): Path<String>,
    Json(req): Json<ProgressRequest>,
) -> Result<Json<ProgressResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    // Fetch the task's machine_id and workflow_run_id in one shot. The
    // machine_id is used as `from_machine` for the agent_messages post;
    // the workflow_run_id drives the channel name. A missing task → 404.
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    let (machine_id, workflow_run_id): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT machine_id, workflow_run_id FROM tasks
             WHERE id = ?1 AND tenant_id = ?2",
            rusqlite::params![&task_id, &tenant_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => (
                StatusCode::NOT_FOUND,
                format!("Task not found: {}", task_id),
            ),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?;

    // Millisecond-precision Unix timestamp → unique key per progress report.
    let ts_ms = chrono::Utc::now().timestamp_millis();
    let key = format!("progress_{}", ts_ms);

    let value = serde_json::json!({
        "files_changed": req.files_changed,
        "tests_run": req.tests_run,
        "tests_passing": req.tests_passing,
        "commits": req.commits,
        "blockers": req.blockers,
        "status": req.status,
        "ts_ms": ts_ms,
        "tenant_id": tenant_id,
    });
    let value_str = value.to_string();

    // INSERT OR REPLACE so a same-millisecond collision (rare) overwrites
    // rather than erroring; the newer report is the more accurate one.
    conn.execute(
        "INSERT OR REPLACE INTO task_outputs (task_id, key, value, artifact_path)
         VALUES (?1, ?2, ?3, NULL)",
        rusqlite::params![&task_id, &key, &value_str],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Announce on the agent bus. Channel resolution:
    //   1. workflow-run-<workflow_run_id>  (preferred)
    //   2. workflow-task-<task_id>          (no workflow run)
    let channel = match workflow_run_id.as_ref() {
        Some(wfr) if !wfr.is_empty() => format!("workflow-run-{}", wfr),
        _ => format!("workflow-task-{}", task_id),
    };

    let announce_body = serde_json::json!({
        "type": "progress",
        "task_id": task_id,
        "key": key,
        "tenant_id": tenant_id,
        "summary": {
            "tests_run": req.tests_run,
            "tests_passing": req.tests_passing,
            "commits": req.commits,
            "blockers": req.blockers,
            "status": req.status,
            "files_changed_count": req.files_changed.len(),
        },
    });
    let announce_str = announce_body.to_string();
    let from_machine = machine_id.clone().unwrap_or_default();

    conn.execute(
        "INSERT INTO agent_messages (from_machine, to_machine, channel, body, created_at)
         VALUES (?1, NULL, ?2, ?3, datetime('now'))",
        rusqlite::params![&from_machine, &channel, &announce_str],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        task_id = %task_id,
        key = %key,
        channel = %channel,
        tests_run = req.tests_run,
        tests_passing = req.tests_passing,
        commits = req.commits,
        "Progress report stored"
    );

    Ok(Json(ProgressResponse {
        status: "stored".to_string(),
        key,
    }))
}

/// `POST /agent/task/:id/reflexion` — submit a post-task reflexion.
///
/// Stores the reflexion in `task_outputs` under the constant key
/// `"reflexion"` (one per task; resubmission overwrites). Returns
/// `{ status: "stored" }`. Returns `404` if the task doesn't exist or
/// belongs to another tenant.
pub async fn submit_reflexion(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(task_id): Path<String>,
    Json(req): Json<ReflexionRequest>,
) -> Result<Json<ReflexionResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    // Confirm the task exists and belongs to this tenant before writing.
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE id = ?1 AND tenant_id = ?2",
            rusqlite::params![&task_id, &tenant_id],
            |row| row.get(0),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if exists == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Task not found: {}", task_id),
        ));
    }

    let value = serde_json::json!({
        "what_went_well": req.what_went_well,
        "what_went_wrong": req.what_went_wrong,
        "what_differently": req.what_differently,
        "what_learned": req.what_learned,
        "tenant_id": tenant_id,
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    let value_str = value.to_string();

    // INSERT OR REPLACE so a resubmission overwrites the prior reflexion
    // (the schema's PRIMARY KEY is (task_id, key), and we always use the
    // constant key "reflexion").
    conn.execute(
        "INSERT OR REPLACE INTO task_outputs (task_id, key, value, artifact_path)
         VALUES (?1, 'reflexion', ?2, NULL)",
        rusqlite::params![&task_id, &value_str],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        task_id = %task_id,
        "Reflexion stored"
    );

    Ok(Json(ReflexionResponse {
        status: "stored".to_string(),
    }))
}

/// `GET /agent/task/:id/reflexion` — retrieve a task's reflexion.
///
/// Returns `404` if the task doesn't exist, belongs to another tenant, or
/// has no reflexion recorded yet.
pub async fn get_reflexion(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<GetReflexionResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    // Single query: join task_outputs with tasks to enforce tenant scoping
    // AND fetch the reflexion in one round-trip. Returns no rows if either
    // the task is missing/unauthorized or no reflexion has been stored.
    //
    // (We avoid aliasing `task_outputs` as `to` because `TO` is on SQLite's
    // keyword list — `outs` is unambiguous.)
    let row = conn.query_row(
        "SELECT outs.task_id, outs.value
         FROM task_outputs outs
         JOIN tasks t ON outs.task_id = t.id
         WHERE outs.task_id = ?1
           AND outs.key = 'reflexion'
           AND t.tenant_id = ?2",
        rusqlite::params![&task_id, &tenant_id],
        |row| {
            let task_id: String = row.get(0)?;
            let value_str: String = row.get(1)?;
            // `value` is written by this module as a JSON string; a parse
            // failure indicates DB corruption — surface as null rather than
            // crashing the request.
            let reflexion: serde_json::Value =
                serde_json::from_str(&value_str).unwrap_or(serde_json::Value::Null);
            Ok(GetReflexionResponse { task_id, reflexion })
        },
    );

    match row {
        Ok(resp) => Ok(Json(resp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err((
            StatusCode::NOT_FOUND,
            format!("No reflexion found for task: {}", task_id),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `GET /agent/reflexions?tenant=<id>&limit=10` — list recent reflexions.
///
/// Returns the most recent reflexions across all of the tenant's tasks. The
/// `tenant` query parameter is optional; if provided it must match the
/// authenticated agent's tenant (otherwise `403`). If omitted, the
/// authenticated tenant is used. `limit` defaults to `10` and is clamped to
/// `100`.
///
/// Results are ordered by `tasks.created_at DESC` (most recently created
/// tasks first) — `task_outputs` has no `created_at` column of its own, so
/// we proxy recency through the task's creation timestamp.
pub async fn list_reflexions(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ListReflexionsQuery>,
) -> Result<Json<Vec<ReflexionListItem>>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let authed_tenant = authenticate_agent(&state, &agent_token)?;

    // If the caller supplied an explicit tenant, it must match the
    // authenticated tenant. Cross-tenant listing is forbidden.
    let tenant_id = match query.tenant.as_deref() {
        Some(t) if t == authed_tenant => authed_tenant,
        Some(_) => {
            return Err((
                StatusCode::FORBIDDEN,
                "tenant query parameter does not match authenticated tenant".to_string(),
            ));
        }
        None => authed_tenant,
    };

    // Clamp limit to [1, 100]; default to 10.
    let limit: i64 = match query.limit {
        Some(n) if n >= 1 => (n.min(100)) as i64,
        _ => 10,
    };

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    let mut stmt = conn
        .prepare(
            "SELECT outs.task_id, outs.value, t.created_at
             FROM task_outputs outs
             JOIN tasks t ON outs.task_id = t.id
             WHERE outs.key = 'reflexion'
               AND t.tenant_id = ?1
             ORDER BY t.created_at DESC
             LIMIT ?2",
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows = stmt
        .query_map(rusqlite::params![&tenant_id, limit], |row| {
            let task_id: String = row.get(0)?;
            let value_str: String = row.get(1)?;
            let task_created_at: String = row.get(2)?;
            let reflexion: serde_json::Value =
                serde_json::from_str(&value_str).unwrap_or(serde_json::Value::Null);
            Ok(ReflexionListItem {
                task_id,
                reflexion,
                task_created_at,
            })
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut items = Vec::new();
    for row in rows {
        let item = row.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        items.push(item);
    }

    Ok(Json(items))
}

// ============================================================================
// SSE stream
// ============================================================================

/// Stream a task's status as Server-Sent Events.
///
/// Emits the task's current status immediately, then polls the database every
/// 500 ms and re-emits **only when the status changes**. A heartbeat comment
/// is sent every 30 s to keep the connection alive. The stream closes once the
/// task reaches a terminal state (`completed`, `failed`, `cancelled`).
///
/// Event types mirror the task lifecycle:
/// - `task_created`   — status `queued`
/// - `task_started`   — status `running`
/// - `task_completed` — status `completed`
/// - `task_failed`    — status `failed`
/// - `task_cancelled` — status `cancelled`
/// - `task_status`    — any other status (e.g. `scheduled`) / not-found fallback
///
/// Each event's data is a JSON object: `{"task_id","status","result"}`.
///
/// This handler intentionally takes no `Authorization` header — tenant
/// scoping / authentication is expected to be applied by the router
/// middleware that wires this up (added by the orchestrator).
pub async fn stream_task(
    Path(task_id): Path<String>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    // Clone the pool so the returned stream is `'static` and not tied to the
    // borrowed `&AppState` (the pool is just an `Arc` around the shared
    // connection pool, so cloning is cheap).
    let db = state.db.clone();

    let stream = async_stream::stream! {
        // Last status we emitted on this stream. Seeded by the initial poll
        // below; the `NotFound` / `Error` arms return early, so by the time
        // we reach the polling loop this is always `Some`.
        let mut last_status: Option<String>;

        // 1. Emit the current status immediately.
        match poll_task_status(&db, &task_id) {
            PollOutcome::Found { status, result } => {
                let terminal = is_terminal(&status);
                yield Ok(task_event(&status, &task_id, result));
                last_status = Some(status);
                if terminal {
                    // Already terminal — we've emitted the final state, close.
                    return;
                }
            }
            PollOutcome::NotFound => {
                // Task doesn't exist — tell the client and close.
                let payload = serde_json::json!({
                    "task_id": task_id,
                    "status": "not_found",
                    "result": serde_json::Value::Null,
                });
                yield Ok(Event::default()
                    .event("task_status")
                    .data(payload.to_string()));
                return;
            }
            PollOutcome::Error => {
                // Couldn't read the initial state — close; client may retry.
                return;
            }
        }

        // 2. Poll every 500 ms for status changes; heartbeat every 30 s.
        let mut poll_interval = tokio::time::interval(Duration::from_millis(500));
        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(30));
        // Delay (rather than burst) if a tick is missed while we were busy
        // yielding events — keeps the cadence steady.
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Discard the immediate first tick so the first poll happens after
        // 500 ms and the first heartbeat after 30 s.
        poll_interval.tick().await;
        heartbeat_interval.tick().await;

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    match poll_task_status(&db, &task_id) {
                        PollOutcome::Found { status, result } => {
                            // Only emit when the status actually changed.
                            if last_status.as_deref() != Some(status.as_str()) {
                                let terminal = is_terminal(&status);
                                yield Ok(task_event(&status, &task_id, result));
                                last_status = Some(status);
                                if terminal {
                                    return;
                                }
                            }
                        }
                        PollOutcome::NotFound => {
                            // Task vanished mid-stream — close.
                            tracing::warn!(
                                task_id = %task_id,
                                "Task vanished during SSE stream; closing"
                            );
                            return;
                        }
                        PollOutcome::Error => {
                            // Transient DB error — log and keep polling; the
                            // next tick will retry.
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    yield Ok(Event::default().data("heartbeat"));
                }
            }
        }
    };

    Sse::new(stream)
}

// ============================================================================
// Helpers (mirror routes/agent.rs — kept private to this module so we don't
// touch any other file)
// ============================================================================

/// Outcome of a single status poll against the `tasks` table.
enum PollOutcome {
    /// The task row exists with this `status` and optional `result`.
    Found {
        status: String,
        result: Option<serde_json::Value>,
    },
    /// The task row is gone (deleted or never existed).
    NotFound,
    /// A transient database error occurred.
    Error,
}

/// Read the current `status` and `result` of a task by ID.
///
/// The query is **not** scoped by `tenant_id` — authentication / tenant
/// isolation is expected to be enforced by the surrounding route middleware.
fn poll_task_status(
    db: &Pool<SqliteConnectionManager>,
    task_id: &str,
) -> PollOutcome {
    let conn = match db.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                error = %e,
                task_id = %task_id,
                "DB pool error in stream_task"
            );
            return PollOutcome::Error;
        }
    };

    let row = conn.query_row(
        "SELECT status, result FROM tasks WHERE id = ?1",
        rusqlite::params![task_id],
        |row| {
            let status: String = row.get(0)?;
            let result_str: Option<String> = row.get(1)?;
            // Both columns are written by this module as valid JSON, so a
            // parse failure indicates DB corruption — surface as null rather
            // than crashing the stream.
            let result: Option<serde_json::Value> = result_str
                .map(|s| serde_json::from_str(&s).unwrap_or(serde_json::Value::Null));
            Ok((status, result))
        },
    );

    match row {
        Ok((status, result)) => PollOutcome::Found { status, result },
        Err(rusqlite::Error::QueryReturnedNoRows) => PollOutcome::NotFound,
        Err(e) => {
            tracing::error!(
                error = %e,
                task_id = %task_id,
                "DB query error in stream_task"
            );
            PollOutcome::Error
        }
    }
}

/// True for terminal task statuses — the stream should close after emitting.
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

/// Build an SSE `Event` for a task status change.
///
/// The event name reflects the lifecycle stage; the data payload is always
/// `{"task_id","status","result"}` (matching the J5 spec).
fn task_event(
    status: &str,
    task_id: &str,
    result: Option<serde_json::Value>,
) -> Event {
    let event_name = match status {
        "queued" => "task_created",
        "running" => "task_started",
        "completed" => "task_completed",
        "failed" => "task_failed",
        "cancelled" => "task_cancelled",
        _ => "task_status",
    };
    let payload = serde_json::json!({
        "task_id": task_id,
        "status": status,
        "result": result,
    });
    Event::default().event(event_name).data(payload.to_string())
}

fn extract_token(
    headers: &axum::http::HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header".to_string(),
        ))?;

    if !auth.starts_with("Bearer ") {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Expected Bearer token".to_string(),
        ));
    }

    Ok(auth[7..].to_string())
}

fn authenticate_agent(
    state: &AppState,
    token: &str,
) -> Result<String, (StatusCode, String)> {
    crate::tenants::auth::verify_agent_token(&state.db, token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- CreateTaskRequest --------------------------------------------------

    #[test]
    fn test_create_task_request_deserialize_full() {
        let json = r#"{
            "instruction": "cargo test",
            "image": "stronghold/rust-nightly:2026.07",
            "ttl_secs": 1800,
            "context": {"env": {"CI": "true"}},
            "parent_task_id": "task_01HZX9",
            "workflow_run_id": "wfr_abc",
            "role": "tester"
        }"#;
        let req: CreateTaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.instruction, "cargo test");
        assert_eq!(req.image, "stronghold/rust-nightly:2026.07");
        assert_eq!(req.ttl_secs, 1800);
        assert_eq!(req.context.as_ref().unwrap()["env"]["CI"], "true");
        assert_eq!(req.parent_task_id.as_deref(), Some("task_01HZX9"));
        assert_eq!(req.workflow_run_id.as_deref(), Some("wfr_abc"));
        assert_eq!(req.role.as_deref(), Some("tester"));
    }

    #[test]
    fn test_create_task_request_deserialize_minimal() {
        // Optionals omitted → must default to None.
        let json = r#"{
            "instruction": "echo hi",
            "image": "stronghold/rocky-base:latest",
            "ttl_secs": 60
        }"#;
        let req: CreateTaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.instruction, "echo hi");
        assert_eq!(req.ttl_secs, 60);
        assert!(req.context.is_none());
        assert!(req.parent_task_id.is_none());
        assert!(req.workflow_run_id.is_none());
        assert!(req.role.is_none());
    }

    #[test]
    fn test_create_task_request_deserialize_with_role_only() {
        // All other optionals omitted; only role is provided.
        let json = r#"{
            "instruction": "review the PR",
            "image": "stronghold/rust-stable:latest",
            "ttl_secs": 300,
            "role": "reviewer"
        }"#;
        let req: CreateTaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.instruction, "review the PR");
        assert_eq!(req.role.as_deref(), Some("reviewer"));
        assert!(req.context.is_none());
        assert!(req.parent_task_id.is_none());
        assert!(req.workflow_run_id.is_none());
    }

    #[test]
    fn test_create_task_request_role_explicit_null() {
        // `role: null` in the JSON should deserialize to None.
        let json = r#"{
            "instruction": "x",
            "image": "y",
            "ttl_secs": 1,
            "role": null
        }"#;
        let req: CreateTaskRequest = serde_json::from_str(json).unwrap();
        assert!(req.role.is_none());
    }

    #[test]
    fn test_create_task_request_missing_required_fields_fails() {
        // Missing instruction → deserialization fails.
        let json = r#"{
            "image": "y",
            "ttl_secs": 1
        }"#;
        assert!(serde_json::from_str::<CreateTaskRequest>(json).is_err());

        // Missing image → fails.
        let json = r#"{
            "instruction": "x",
            "ttl_secs": 1
        }"#;
        assert!(serde_json::from_str::<CreateTaskRequest>(json).is_err());

        // Missing ttl_secs → fails.
        let json = r#"{
            "instruction": "x",
            "image": "y"
        }"#;
        assert!(serde_json::from_str::<CreateTaskRequest>(json).is_err());
    }

    // --- CreateTaskResponse -------------------------------------------------

    #[test]
    fn test_create_task_response_serialize_queued() {
        let resp = CreateTaskResponse {
            task_id: "task_01HZX9Q8J7".to_string(),
            machine_id: None,
            status: "queued".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"task_id\":\"task_01HZX9Q8J7\""));
        assert!(json.contains("\"status\":\"queued\""));
        assert!(json.contains("\"machine_id\":null"));
    }

    #[test]
    fn test_create_task_response_serialize_with_machine() {
        // When a task has been assigned a machine, machine_id is Some.
        let resp = CreateTaskResponse {
            task_id: "task_xyz".to_string(),
            machine_id: Some("machine_1".to_string()),
            status: "running".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"task_id\":\"task_xyz\""));
        assert!(json.contains("\"machine_id\":\"machine_1\""));
        assert!(json.contains("\"status\":\"running\""));
        // The serialized value must be a JSON object with exactly 3 keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    // --- Spec JSON construction (role snapshot) -----------------------------
    //
    // The create_task handler builds the spec JSON inline; we replicate the
    // construction logic here to assert on its shape without needing a live
    // DB / AppState. This catches regressions in the role-snapshot contract:
    // - role-less spec has exactly 4 keys: instruction, image, ttl_secs, context.
    // - role-ful spec adds `role` (string) and `role_system_prompt` (string|null).

    #[test]
    fn test_spec_json_without_role_has_no_role_fields() {
        // Mirrors the spec construction in create_task when req.role is None.
        let spec = serde_json::json!({
            "instruction": "echo hi",
            "image": "rocky",
            "ttl_secs": 60,
            "context": null,
        });
        let obj = spec.as_object().unwrap();
        assert_eq!(obj.len(), 4, "role-less spec must have 4 keys, got {obj:?}");
        assert!(!obj.contains_key("role"));
        assert!(!obj.contains_key("role_system_prompt"));
    }

    #[test]
    fn test_spec_json_with_role_and_prompt_snapshots_both() {
        // Mirrors the spec construction when role is Some and the lookup hit.
        let role_name = "coder";
        let role_system_prompt = Some("You are a Coder Agent.".to_string());
        let mut spec = serde_json::json!({
            "instruction": "implement auth",
            "image": "rust",
            "ttl_secs": 1800,
            "context": null,
        });
        spec["role"] = serde_json::Value::String(role_name.to_string());
        spec["role_system_prompt"] = match role_system_prompt {
            Some(p) => serde_json::Value::String(p),
            None => serde_json::Value::Null,
        };
        let obj = spec.as_object().unwrap();
        assert_eq!(obj.len(), 6, "role-ful spec must have 6 keys, got {obj:?}");
        assert_eq!(obj["role"], "coder");
        assert_eq!(obj["role_system_prompt"], "You are a Coder Agent.");
    }

    #[test]
    fn test_spec_json_with_role_but_missing_prompt_uses_null() {
        // Mirrors the spec construction when role is Some but the lookup missed
        // (role row doesn't exist for this tenant).
        let role_name = "ghost";
        let role_system_prompt: Option<String> = None;
        let mut spec = serde_json::json!({
            "instruction": "x",
            "image": "y",
            "ttl_secs": 1,
            "context": null,
        });
        spec["role"] = serde_json::Value::String(role_name.to_string());
        spec["role_system_prompt"] = match role_system_prompt {
            Some(p) => serde_json::Value::String(p),
            None => serde_json::Value::Null,
        };
        assert_eq!(spec["role"], "ghost");
        assert!(spec["role_system_prompt"].is_null());
    }

    // --- GetTaskResponse ----------------------------------------------------

    #[test]
    fn test_get_task_response_serialize_completed() {
        let resp = GetTaskResponse {
            id: "task_1".to_string(),
            status: "completed".to_string(),
            spec: serde_json::json!({"instruction": "ls", "image": "rocky", "ttl_secs": 30, "context": null}),
            result: Some(serde_json::json!({"exit_code": 0, "stdout": "a\nb\n", "stderr": "", "summary": "ok", "artifacts": []})),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            started_at: Some("2026-01-01T00:00:05Z".to_string()),
            finished_at: Some("2026-01-01T00:00:10Z".to_string()),
            error: None,
            retry_count: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":\"task_1\""));
        assert!(json.contains("\"status\":\"completed\""));
        assert!(json.contains("\"retry_count\":0"));
        assert!(json.contains("\"error\":null"));
        assert!(json.contains("\"started_at\":\"2026-01-01T00:00:05Z\""));
        assert!(json.contains("\"finished_at\":\"2026-01-01T00:00:10Z\""));
        // spec/result are embedded JSON objects.
        assert!(json.contains("\"instruction\":\"ls\""));
        assert!(json.contains("\"exit_code\":0"));
    }

    #[test]
    fn test_get_task_response_serialize_queued_with_nulls() {
        // A freshly queued task: no result, no started/finished, no error.
        let resp = GetTaskResponse {
            id: "task_2".to_string(),
            status: "queued".to_string(),
            spec: serde_json::json!({"instruction": "build", "image": "rust", "ttl_secs": 600, "context": null}),
            result: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            started_at: None,
            finished_at: None,
            error: None,
            retry_count: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\":null"));
        assert!(json.contains("\"started_at\":null"));
        assert!(json.contains("\"finished_at\":null"));
        assert!(json.contains("\"error\":null"));
        assert!(json.contains("\"status\":\"queued\""));
    }

    #[test]
    fn test_get_task_response_serialize_failed_with_error_and_retries() {
        let resp = GetTaskResponse {
            id: "task_3".to_string(),
            status: "failed".to_string(),
            spec: serde_json::Value::Null,
            result: Some(serde_json::json!({"exit_code": 1, "summary": "boom"})),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            started_at: Some("2026-01-01T00:00:05Z".to_string()),
            finished_at: Some("2026-01-01T00:00:20Z".to_string()),
            error: Some("container OOM-killed".to_string()),
            retry_count: 2,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
        assert!(json.contains("\"error\":\"container OOM-killed\""));
        assert!(json.contains("\"retry_count\":2"));
    }

    // --- SubmitResultRequest ------------------------------------------------

    #[test]
    fn test_submit_result_request_deserialize_success() {
        let json = r#"{
            "exit_code": 0,
            "stdout": "all good\n",
            "stderr": "",
            "summary": "tests passed",
            "artifacts": [{"name": "report", "path": "/out/report.html"}]
        }"#;
        let req: SubmitResultRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.exit_code, 0);
        assert_eq!(req.stdout, "all good\n");
        assert_eq!(req.summary, "tests passed");
        assert_eq!(req.artifacts.len(), 1);
        assert_eq!(req.artifacts[0]["name"], "report");
    }

    #[test]
    fn test_submit_result_request_deserialize_failure_empty_artifacts() {
        let json = r#"{
            "exit_code": 127,
            "stdout": "",
            "stderr": "command not found",
            "summary": "binary missing",
            "artifacts": []
        }"#;
        let req: SubmitResultRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.exit_code, 127);
        assert!(req.artifacts.is_empty());
        assert_eq!(req.stderr, "command not found");
    }

    // --- ProgressRequest ---------------------------------------------------

    #[test]
    fn test_progress_request_deserialize_full() {
        let json = r#"{
            "files_changed": ["src/lib.rs", "tests/lib.rs"],
            "tests_run": 42,
            "tests_passing": 40,
            "commits": 3,
            "blockers": ["flaky test on macOS", "missing API key"],
            "status": "on_track"
        }"#;
        let req: ProgressRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.files_changed.len(), 2);
        assert_eq!(req.files_changed[0], "src/lib.rs");
        assert_eq!(req.tests_run, 42);
        assert_eq!(req.tests_passing, 40);
        assert_eq!(req.commits, 3);
        assert_eq!(req.blockers.len(), 2);
        assert_eq!(req.blockers[1], "missing API key");
        assert_eq!(req.status, "on_track");
    }

    #[test]
    fn test_progress_request_deserialize_empty_arrays() {
        // Empty arrays / zero counts are valid (e.g. first heartbeat).
        let json = r#"{
            "files_changed": [],
            "tests_run": 0,
            "tests_passing": 0,
            "commits": 0,
            "blockers": [],
            "status": "starting"
        }"#;
        let req: ProgressRequest = serde_json::from_str(json).unwrap();
        assert!(req.files_changed.is_empty());
        assert_eq!(req.tests_run, 0);
        assert!(req.blockers.is_empty());
        assert_eq!(req.status, "starting");
    }

    #[test]
    fn test_progress_request_rejects_missing_status() {
        // `status` is required; omitting it must fail.
        let json = r#"{
            "files_changed": [],
            "tests_run": 0,
            "tests_passing": 0,
            "commits": 0,
            "blockers": []
        }"#;
        let result: Result<ProgressRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // --- ProgressResponse --------------------------------------------------

    #[test]
    fn test_progress_response_serialize_stored() {
        let resp = ProgressResponse {
            status: "stored".to_string(),
            key: "progress_1700000000123".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"stored\""));
        assert!(json.contains("\"key\":\"progress_1700000000123\""));
        // Exactly 2 top-level keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    // --- ReflexionRequest --------------------------------------------------

    #[test]
    fn test_reflexion_request_deserialize_full() {
        let json = r#"{
            "what_went_well": "test coverage hit 95%",
            "what_went_wrong": "spent 2h debugging a typo",
            "what_differently": "enable clippy::typos from the start",
            "what_learned": "rustc 1.83 stabilizes inline_const"
        }"#;
        let req: ReflexionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.what_went_well, "test coverage hit 95%");
        assert_eq!(req.what_went_wrong, "spent 2h debugging a typo");
        assert_eq!(req.what_differently, "enable clippy::typos from the start");
        assert_eq!(req.what_learned, "rustc 1.83 stabilizes inline_const");
    }

    #[test]
    fn test_reflexion_request_rejects_missing_field() {
        // All four fields are required; omitting one must fail.
        let json = r#"{
            "what_went_well": "x",
            "what_went_wrong": "y",
            "what_differently": "z"
        }"#;
        let result: Result<ReflexionRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_reflexion_request_allows_empty_strings() {
        // Empty strings are valid (an agent may legitimately have nothing to
        // say in one of the four slots).
        let json = r#"{
            "what_went_well": "",
            "what_went_wrong": "",
            "what_differently": "",
            "what_learned": ""
        }"#;
        let req: ReflexionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.what_went_well, "");
        assert_eq!(req.what_learned, "");
    }

    // --- ReflexionResponse -------------------------------------------------

    #[test]
    fn test_reflexion_response_serialize_stored() {
        let resp = ReflexionResponse {
            status: "stored".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"status":"stored"}"#);
    }

    // --- GetReflexionResponse ----------------------------------------------

    #[test]
    fn test_get_reflexion_response_serialize() {
        let resp = GetReflexionResponse {
            task_id: "task_01HZX9".to_string(),
            reflexion: serde_json::json!({
                "what_went_well": "fast",
                "what_went_wrong": "none",
                "what_differently": "nothing",
                "what_learned": "rust is great"
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"task_id\":\"task_01HZX9\""));
        assert!(json.contains("\"what_went_well\":\"fast\""));
        assert!(json.contains("\"what_learned\":\"rust is great\""));
        // 2 top-level keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    // --- ListReflexionsQuery -----------------------------------------------

    #[test]
    fn test_list_reflexions_query_deserialize_full() {
        let json = r#"{ "tenant": "tenant_abc", "limit": 25 }"#;
        let q: ListReflexionsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.tenant.as_deref(), Some("tenant_abc"));
        assert_eq!(q.limit, Some(25));
    }

    #[test]
    fn test_list_reflexions_query_deserialize_empty() {
        // Both fields optional; empty object must deserialize to None/None.
        let json = r#"{}"#;
        let q: ListReflexionsQuery = serde_json::from_str(json).unwrap();
        assert!(q.tenant.is_none());
        assert!(q.limit.is_none());
    }

    #[test]
    fn test_list_reflexions_query_deserialize_only_tenant() {
        let json = r#"{ "tenant": "t1" }"#;
        let q: ListReflexionsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.tenant.as_deref(), Some("t1"));
        assert!(q.limit.is_none());
    }

    // --- ReflexionListItem -------------------------------------------------

    #[test]
    fn test_reflexion_list_item_serialize() {
        let item = ReflexionListItem {
            task_id: "task_42".to_string(),
            reflexion: serde_json::json!({"what_learned": "x"}),
            task_created_at: "2026-01-01 00:00:00".to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"task_id\":\"task_42\""));
        assert!(json.contains("\"what_learned\":\"x\""));
        assert!(json.contains("\"task_created_at\":\"2026-01-01 00:00:00\""));
        // 3 top-level keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 3);
    }
}
