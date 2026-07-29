//! Workflow definition + run lifecycle endpoints.
//!
//! A **workflow** is a named, versioned DAG of steps stored in the
//! `workflows` table. A **workflow run** is a single execution instance of
//! a workflow — a row in `workflow_runs` — driven by the [`engine`].
//!
//! # Endpoints
//! - `POST   /workflow`         — define a new workflow (`status = draft`)
//! - `GET    /workflow/:id`     — fetch a workflow definition
//! - `GET    /workflow`         — list workflows for the caller's tenant
//! - `POST   /workflow/:id/run` — start a run; spawns the engine in the
//!   background via `tokio::spawn`
//! - `GET    /workflow/run/:id` — poll a run's status / step progress
//!
//! All endpoints require a valid agent bearer token (tenant-scoped). The
//! `tenant_id` is taken from the token, never from the request body, so a
//! tenant can only see / run its own workflows.
//!
//! # Route wiring
//! The `.route(...)` calls live in [`crate::routes::mod`] — this module only
//! provides the handler functions. The orchestrator wires them into the
//! router.
//!
//! [`engine`]: crate::workflow::engine
//! [`crate::routes::mod`]: crate::routes

use crate::routes::AppState;
use crate::tenants::auth::verify_agent_token;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

// ============================================================================
// Request / response types
// ============================================================================

/// Request body for `POST /workflow`.
#[derive(Debug, Deserialize)]
pub struct CreateWorkflowRequest {
    /// Human-readable workflow name (e.g. `"ci-build"`).
    pub name: String,
    /// The DAG definition as a JSON object: `{ "steps": [...] }`.
    /// Stored verbatim — the engine parses it at run time.
    pub dag: serde_json::Value,
}

/// Response body for `POST /workflow`.
#[derive(Debug, Serialize)]
pub struct CreateWorkflowResponse {
    pub workflow_id: String,
    pub status: String,
}

/// Response body for `GET /workflow/:id`.
#[derive(Debug, Serialize)]
pub struct GetWorkflowResponse {
    pub id: String,
    pub name: String,
    pub dag: serde_json::Value,
    pub status: String,
    pub created_at: String,
}

/// One row in the `GET /workflow` list response.
#[derive(Debug, Serialize)]
pub struct WorkflowSummary {
    pub id: String,
    pub name: String,
    pub status: String,
    pub created_at: String,
}

/// Response body for `POST /workflow/:id/run`.
#[derive(Debug, Serialize)]
pub struct RunWorkflowResponse {
    pub run_id: String,
    pub status: String,
}

/// Response body for `GET /workflow/run/:id`.
#[derive(Debug, Serialize)]
pub struct GetRunResponse {
    pub id: String,
    pub workflow_id: String,
    pub status: String,
    /// JSON array of step IDs currently being executed. Empty once the run
    /// reaches a terminal state.
    pub current_steps: serde_json::Value,
    /// JSON array of step IDs that have completed (or been skipped).
    pub completed_steps: serde_json::Value,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

// ============================================================================
// Handlers
// ============================================================================

/// `POST /workflow` — define a new workflow.
///
/// Stores the DAG JSON verbatim in the `workflows` table with
/// `status = 'draft'`. Returns the new workflow ID. The DAG is not validated
/// at definition time — structural errors (cycles, missing deps) surface
/// only when a run is started.
pub async fn create_workflow(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateWorkflowRequest>,
) -> Result<Json<CreateWorkflowResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let workflow_id = format!("wf_{}", ulid::Ulid::new());
    let dag_str = req.dag.to_string();

    tracing::info!(
        tenant = %tenant_id,
        workflow_id = %workflow_id,
        name = %req.name,
        "Workflow defined (draft)"
    );

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    conn.execute(
        "INSERT INTO workflows (id, tenant_id, name, dag, status, created_at)
         VALUES (?1, ?2, ?3, ?4, 'draft', datetime('now'))",
        rusqlite::params![workflow_id, tenant_id, req.name, dag_str],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(CreateWorkflowResponse {
        workflow_id,
        status: "draft".to_string(),
    }))
}

/// `GET /workflow/:id` — fetch a workflow definition.
///
/// Returns `404` if the workflow does not exist or belongs to a different
/// tenant.
pub async fn get_workflow(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(workflow_id): Path<String>,
) -> Result<Json<GetWorkflowResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let row = conn.query_row(
        "SELECT id, name, dag, status, created_at
         FROM workflows
         WHERE id = ?1 AND tenant_id = ?2",
        rusqlite::params![workflow_id, tenant_id],
        |row| {
            let dag_str: String = row.get(2)?;
            let dag: serde_json::Value =
                serde_json::from_str(&dag_str).unwrap_or(serde_json::Value::Null);
            Ok(GetWorkflowResponse {
                id: row.get(0)?,
                name: row.get(1)?,
                dag,
                status: row.get(3)?,
                created_at: row.get(4)?,
            })
        },
    );

    match row {
        Ok(resp) => Ok(Json(resp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err((
            StatusCode::NOT_FOUND,
            format!("Workflow not found: {}", workflow_id),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `GET /workflow` — list all workflows for the caller's tenant.
///
/// Ordered by `created_at` descending (newest first). Returns only metadata
/// (no DAG body) to keep the payload small.
pub async fn list_workflows(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<WorkflowSummary>>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, name, status, created_at
             FROM workflows
             WHERE tenant_id = ?1
             ORDER BY created_at DESC",
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let workflows: Vec<WorkflowSummary> = stmt
        .query_map(rusqlite::params![tenant_id], |row| {
            Ok(WorkflowSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                status: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(workflows))
}

/// `POST /workflow/:id/run` — start a workflow run.
///
/// Creates a `workflow_runs` row with `status = 'running'`, then spawns the
/// [`engine::execute`] coroutine in the background via `tokio::spawn`. The
/// HTTP response returns immediately with the run ID; the client polls
/// `GET /workflow/run/:id` for progress.
///
/// Returns `404` if the workflow doesn't exist or belongs to another tenant.
///
/// [`engine::execute`]: crate::workflow::engine::execute
pub async fn run_workflow(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(workflow_id): Path<String>,
) -> Result<Json<RunWorkflowResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    // Confirm the workflow exists and belongs to this tenant before creating
    // the run row. A missing workflow → 404.
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM workflows WHERE id = ?1 AND tenant_id = ?2",
            rusqlite::params![workflow_id, tenant_id],
            |row| row.get(0),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if exists == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Workflow not found: {}", workflow_id),
        ));
    }

    let run_id = format!("wfr_{}", ulid::Ulid::new());
    conn.execute(
        "INSERT INTO workflow_runs
         (id, workflow_id, tenant_id, status, current_steps, completed_steps,
          started_at)
         VALUES (?1, ?2, ?3, 'running', '[]', '[]', datetime('now'))",
        rusqlite::params![run_id, workflow_id, tenant_id],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    drop(conn);

    tracing::info!(
        tenant = %tenant_id,
        workflow_id = %workflow_id,
        run_id = %run_id,
        "Workflow run started"
    );

    // Spawn the DAG executor in the background. It owns its own cloned
    // AppState (DB pool + keys) and runs independently of this request.
    let state_clone = state.clone();
    let run_id_clone = run_id.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::workflow::engine::execute(&run_id_clone, state_clone).await {
            tracing::error!(
                run_id = %run_id_clone,
                error = %e,
                "Workflow execution failed"
            );
        }
    });

    Ok(Json(RunWorkflowResponse {
        run_id,
        status: "running".to_string(),
    }))
}

/// `GET /workflow/run/:id` — poll a workflow run's status.
///
/// Returns the run's status, current/completed step arrays, and timestamps.
/// Returns `404` if the run doesn't exist or belongs to another tenant.
pub async fn get_run(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Result<Json<GetRunResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let row = conn.query_row(
        "SELECT id, workflow_id, status, current_steps, completed_steps,
                started_at, finished_at
         FROM workflow_runs
         WHERE id = ?1 AND tenant_id = ?2",
        rusqlite::params![run_id, tenant_id],
        |row| {
            let current_str: Option<String> = row.get(3)?;
            let completed_str: Option<String> = row.get(4)?;
            let current_steps: serde_json::Value = current_str
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Array(vec![]));
            let completed_steps: serde_json::Value = completed_str
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Array(vec![]));
            Ok(GetRunResponse {
                id: row.get(0)?,
                workflow_id: row.get(1)?,
                status: row.get(2)?,
                current_steps,
                completed_steps,
                started_at: row.get(5)?,
                finished_at: row.get(6)?,
            })
        },
    );

    match row {
        Ok(resp) => Ok(Json(resp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err((
            StatusCode::NOT_FOUND,
            format!("Workflow run not found: {}", run_id),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// ============================================================================
// Auth helpers (mirror routes/tasks.rs — kept private so we don't touch any
// other file)
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
    verify_agent_token(&state.db, token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- CreateWorkflowRequest --------------------------------------------

    #[test]
    fn test_create_workflow_request_deserialize() {
        let json = r#"{
            "name": "ci-build",
            "dag": {
                "steps": [
                    {"id": "build", "task": "cargo build"},
                    {"id": "test", "task": "cargo test", "depends_on": ["build"]}
                ]
            }
        }"#;
        let req: CreateWorkflowRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "ci-build");
        assert!(req.dag.is_object());
        assert!(req.dag.get("steps").is_some());
    }

    #[test]
    fn test_create_workflow_request_deserialize_minimal_dag() {
        let json = r#"{
            "name": "noop",
            "dag": {"steps": []}
        }"#;
        let req: CreateWorkflowRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "noop");
        assert_eq!(req.dag["steps"].as_array().unwrap().len(), 0);
    }

    // --- CreateWorkflowResponse -------------------------------------------

    #[test]
    fn test_create_workflow_response_serialize() {
        let resp = CreateWorkflowResponse {
            workflow_id: "wf_01HZX9".to_string(),
            status: "draft".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"workflow_id\":\"wf_01HZX9\""));
        assert!(json.contains("\"status\":\"draft\""));
        // Exactly 2 fields.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    // --- GetWorkflowResponse ----------------------------------------------

    #[test]
    fn test_get_workflow_response_serialize() {
        let resp = GetWorkflowResponse {
            id: "wf_1".to_string(),
            name: "ci".to_string(),
            dag: serde_json::json!({"steps": [{"id": "a", "task": "a"}]}),
            status: "draft".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":\"wf_1\""));
        assert!(json.contains("\"name\":\"ci\""));
        assert!(json.contains("\"status\":\"draft\""));
        // DAG is embedded JSON.
        assert!(json.contains("\"steps\""));
        assert!(json.contains("\"id\":\"a\""));
    }

    // --- WorkflowSummary --------------------------------------------------

    #[test]
    fn test_workflow_summary_serialize() {
        let s = WorkflowSummary {
            id: "wf_1".to_string(),
            name: "ci".to_string(),
            status: "active".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 4);
        assert_eq!(v["id"], "wf_1");
        assert_eq!(v["status"], "active");
    }

    // --- RunWorkflowResponse ----------------------------------------------

    #[test]
    fn test_run_workflow_response_serialize() {
        let resp = RunWorkflowResponse {
            run_id: "wfr_01HZX9".to_string(),
            status: "running".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"run_id\":\"wfr_01HZX9\""));
        assert!(json.contains("\"status\":\"running\""));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    // --- GetRunResponse ---------------------------------------------------

    #[test]
    fn test_get_run_response_serialize_running() {
        let resp = GetRunResponse {
            id: "wfr_1".to_string(),
            workflow_id: "wf_1".to_string(),
            status: "running".to_string(),
            current_steps: serde_json::json!(["build"]),
            completed_steps: serde_json::json!(["fetch"]),
            started_at: Some("2026-01-01T00:00:00Z".to_string()),
            finished_at: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"running\""));
        assert!(json.contains("\"current_steps\":[\"build\"]"));
        assert!(json.contains("\"completed_steps\":[\"fetch\"]"));
        assert!(json.contains("\"finished_at\":null"));
    }

    #[test]
    fn test_get_run_response_serialize_completed() {
        let resp = GetRunResponse {
            id: "wfr_2".to_string(),
            workflow_id: "wf_1".to_string(),
            status: "completed".to_string(),
            current_steps: serde_json::json!([]),
            completed_steps: serde_json::json!(["fetch", "build", "test"]),
            started_at: Some("2026-01-01T00:00:00Z".to_string()),
            finished_at: Some("2026-01-01T00:10:00Z".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"completed\""));
        assert!(json.contains("\"current_steps\":[]"));
        assert!(json.contains("\"finished_at\":\"2026-01-01T00:10:00Z\""));
    }

    #[test]
    fn test_get_run_response_serialize_failed() {
        let resp = GetRunResponse {
            id: "wfr_3".to_string(),
            workflow_id: "wf_1".to_string(),
            status: "failed".to_string(),
            current_steps: serde_json::json!([]),
            completed_steps: serde_json::json!(["fetch"]),
            started_at: Some("2026-01-01T00:00:00Z".to_string()),
            finished_at: Some("2026-01-01T00:05:00Z".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
    }

    // --- Extract token ----------------------------------------------------

    #[test]
    fn test_extract_token_valid_bearer() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer stronghold_agent_xyz".parse().unwrap(),
        );
        let token = extract_token(&headers).unwrap();
        assert_eq!(token, "stronghold_agent_xyz");
    }

    #[test]
    fn test_extract_token_missing_header() {
        let headers = axum::http::HeaderMap::new();
        let err = extract_token(&headers);
        assert!(err.is_err());
        let (status, _) = err.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_extract_token_wrong_scheme() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Basic abc".parse().unwrap());
        let err = extract_token(&headers);
        assert!(err.is_err());
    }

    // --- End-to-end DB integration ----------------------------------------
    //
    // These tests exercise the full create → get → list → run → get-run
    // cycle against an in-memory database. They don't drive the engine to
    // completion (that requires a task scheduler), but they verify the HTTP
    // handlers wire up to the schema correctly.

    /// Build an AppState backed by an in-memory DB for testing. Seeds a
    /// tenant + agent token so handlers can authenticate.
    fn setup_state() -> (AppState, String, String) {
        use crate::crypto::hybrid_kem::PushKeys;
        use crate::crypto::hybrid_sig::AuditKeys;
        use std::sync::Arc;

        let pool = crate::db::init_memory_pool().unwrap();
        let tenant = crate::tenants::registry::create(&pool, "test-tenant").unwrap();
        let token =
            crate::tenants::auth::mint_agent_token(&pool, &tenant.id, "default", 3600).unwrap();

        let state = AppState {
            db: pool,
            audit_keys: AuditKeys::generate(),
            push_keys: PushKeys::generate(),
            pty_registry: Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        };
        (state, tenant.id, token)
    }

    fn auth_headers(token: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            "authorization",
            format!("Bearer {}", token).parse().unwrap(),
        );
        h
    }

    #[tokio::test]
    async fn test_create_and_get_workflow() {
        let (state, _tenant, token) = setup_state();
        let headers = auth_headers(&token);

        // Create.
        let req = CreateWorkflowRequest {
            name: "ci-build".to_string(),
            dag: serde_json::json!({
                "steps": [
                    {"id": "build", "task": "cargo build"},
                    {"id": "test", "task": "cargo test", "depends_on": ["build"]}
                ]
            }),
        };
        let resp = create_workflow(State(state.clone()), headers.clone(), Json(req))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.status, "draft");
        assert!(resp.workflow_id.starts_with("wf_"));
        let wf_id = resp.workflow_id;

        // Get it back.
        let got = get_workflow(State(state.clone()), headers.clone(), Path(wf_id.clone()))
            .await
            .unwrap()
            .0;
        assert_eq!(got.id, wf_id);
        assert_eq!(got.name, "ci-build");
        assert_eq!(got.status, "draft");
        assert_eq!(got.dag["steps"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_get_workflow_not_found() {
        let (state, _tenant, token) = setup_state();
        let headers = auth_headers(&token);

        let err = get_workflow(State(state), headers, Path("wf_doesnotexist".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_workflows() {
        let (state, _tenant, token) = setup_state();
        let headers = auth_headers(&token);

        // Create three workflows.
        for i in 0..3 {
            let req = CreateWorkflowRequest {
                name: format!("wf-{}", i),
                dag: serde_json::json!({"steps": []}),
            };
            create_workflow(State(state.clone()), headers.clone(), Json(req))
                .await
                .unwrap();
        }

        let list = list_workflows(State(state), headers).await.unwrap().0;
        assert_eq!(list.len(), 3);
        // All should have status "draft".
        assert!(list.iter().all(|w| w.status == "draft"));
    }

    #[tokio::test]
    async fn test_list_workflows_empty() {
        let (state, _tenant, token) = setup_state();
        let headers = auth_headers(&token);

        let list = list_workflows(State(state), headers).await.unwrap().0;
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_run_workflow_creates_run_row() {
        let (state, _tenant, token) = setup_state();
        let headers = auth_headers(&token);

        // Create a workflow first.
        let req = CreateWorkflowRequest {
            name: "noop".to_string(),
            dag: serde_json::json!({"steps": []}),
        };
        let wf = create_workflow(State(state.clone()), headers.clone(), Json(req))
            .await
            .unwrap()
            .0;

        // Start a run.
        let run = run_workflow(State(state.clone()), headers.clone(), Path(wf.workflow_id.clone()))
            .await
            .unwrap()
            .0;
        assert_eq!(run.status, "running");
        assert!(run.run_id.starts_with("wfr_"));

        // Give the spawned engine a moment to mark the run as running.
        // (With an empty DAG it will complete near-instantly.)
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Fetch the run status.
        let got = get_run(State(state), headers, Path(run.run_id.clone()))
            .await
            .unwrap()
            .0;
        assert_eq!(got.id, run.run_id);
        assert_eq!(got.workflow_id, wf.workflow_id);
        // Empty DAG → run should reach a terminal state (completed or
        // failed if the engine rejects it).
        let terminal = matches!(got.status.as_str(), "completed" | "failed");
        assert!(terminal, "expected terminal status, got {}", got.status);
        assert!(got.started_at.is_some());
    }

    #[tokio::test]
    async fn test_run_workflow_not_found() {
        let (state, _tenant, token) = setup_state();
        let headers = auth_headers(&token);

        let err = run_workflow(State(state), headers, Path("wf_missing".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_run_not_found() {
        let (state, _tenant, token) = setup_state();
        let headers = auth_headers(&token);

        let err = get_run(State(state), headers, Path("wfr_missing".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_unauthorized_request_rejected() {
        let (state, _tenant, _token) = setup_state();
        let headers = axum::http::HeaderMap::new(); // no auth header

        let err = list_workflows(State(state), headers).await.unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_workflow_tenant_isolation() {
        // A workflow created by tenant A must not be visible to tenant B.
        let (state_a, _tenant_a_id, token_a) = setup_state();
        let (state_b, _tenant_b_id, token_b) = setup_state();

        // Tenant A creates a workflow.
        let req = CreateWorkflowRequest {
            name: "secret".to_string(),
            dag: serde_json::json!({"steps": []}),
        };
        let wf = create_workflow(
            State(state_a.clone()),
            auth_headers(&token_a),
            Json(req),
        )
        .await
        .unwrap()
        .0;

        // Tenant B tries to read it → 404.
        let err = get_workflow(
            State(state_b.clone()),
            auth_headers(&token_b),
            Path(wf.workflow_id.clone()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);

        // Tenant B's list is empty.
        let list = list_workflows(State(state_b), auth_headers(&token_b))
            .await
            .unwrap()
            .0;
        assert!(list.is_empty());

        // Tenant A's list has the one workflow.
        let list = list_workflows(State(state_a), auth_headers(&token_a))
            .await
            .unwrap()
            .0;
        assert_eq!(list.len(), 1);
    }
}
