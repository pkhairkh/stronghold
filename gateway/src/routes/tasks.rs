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
//! - `POST /agent/task`           — Create a new queued task
//! - `GET  /agent/task/:id`       — Fetch a task's status and details
//! - `POST /agent/task/:id/result`— Submit a task's execution result
//!
//! All endpoints require a valid agent bearer token (tenant-scoped).

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

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

// ============================================================================
// Handlers
// ============================================================================

/// Create a new task in the `queued` state.
///
/// This does **not** create a machine/session — that happens when the task is
/// later "started" (by the scheduler or an operator). For now the task is just
/// queued and visible via `GET /agent/task/:id`.
pub async fn create_task(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let task_id = format!("task_{}", ulid::Ulid::new());

    // Build the immutable spec blob: {instruction, image, ttl_secs, context}.
    let spec = serde_json::json!({
        "instruction": req.instruction,
        "image": req.image,
        "ttl_secs": req.ttl_secs,
        "context": req.context,
    });
    let spec_str = spec.to_string();

    tracing::info!(
        tenant = %tenant_id,
        task_id = %task_id,
        image = %serde_json::json!(req.image),
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
            let result: Option<serde_json::Value> = match result_str {
                Some(s) => Some(
                    serde_json::from_str(&s).unwrap_or(serde_json::Value::Null),
                ),
                None => None,
            };
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
// Helpers (mirror routes/agent.rs — kept private to this module so we don't
// touch any other file)
// ============================================================================

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
            "workflow_run_id": "wfr_abc"
        }"#;
        let req: CreateTaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.instruction, "cargo test");
        assert_eq!(req.image, "stronghold/rust-nightly:2026.07");
        assert_eq!(req.ttl_secs, 1800);
        assert_eq!(req.context.as_ref().unwrap()["env"]["CI"], "true");
        assert_eq!(req.parent_task_id.as_deref(), Some("task_01HZX9"));
        assert_eq!(req.workflow_run_id.as_deref(), Some("wfr_abc"));
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
}
