//! Facilitator disagreement-mediation endpoints.
//!
//! When a **Coder** and a **Reviewer** disagree (e.g. the reviewer blocks a PR
//! the coder thinks is fine), either party can submit a **disagreement** to
//! the **Facilitator** — a neutral mediation agent that issues a binding
//! decision. The disagreement is recorded in the `disagreements` table
//! (migration 004) and announced on the agent message bus so the facilitator
//! can pick it up. The submitting agent then polls the GET endpoint until the
//! row's `status` flips from `"pending"` to a terminal state.
//!
//! # Endpoints
//!
//! | Method | Path                                            | Handler                  |
//! |--------|-------------------------------------------------|--------------------------|
//! | POST   | `/agent/:machine_id/disagreement`               | [`submit_disagreement`]  |
//! | GET    | `/agent/:machine_id/disagreement/:id`           | [`get_decision`]         |
//!
//! Both endpoints require a valid agent bearer token (tenant-scoped), supplied
//! via the `Authorization: Bearer <token>` header.

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

// ============================================================================
// Request / response types
// ============================================================================

/// Request body for `POST /agent/:machine_id/disagreement`.
#[derive(Debug, Deserialize)]
pub struct SubmitDisagreementRequest {
    /// The task the disagreement is about. Optional — some disagreements
    /// (e.g. architectural disputes) are not tied to a specific task.
    pub task_id: Option<String>,
    /// Short human-readable summary of the disagreement
    /// (e.g. "PR #42 should be merged despite failing lint").
    pub issue: String,
    /// The coder's argument for their position.
    pub coder_argument: Option<String>,
    /// The reviewer's argument for the opposing position.
    pub reviewer_argument: Option<String>,
    /// Free-form context — relevant code snippets, CI logs, prior decisions,
    /// etc. Stored verbatim as a JSON string in the `disagreements.context`
    /// column.
    pub context: Option<serde_json::Value>,
}

/// Response body for `POST /agent/:machine_id/disagreement`.
#[derive(Debug, Serialize)]
pub struct SubmitDisagreementResponse {
    /// The newly minted disagreement ID (`dg_<ULID>`).
    pub disagreement_id: String,
    /// Always `"pending"` — the disagreement has been recorded and is
    /// awaiting a facilitator decision.
    pub status: String,
}

/// Response body for `GET /agent/:machine_id/disagreement/:id`.
#[derive(Debug, Serialize)]
pub struct GetDecisionResponse {
    /// The disagreement ID being polled.
    pub id: String,
    /// `"pending"` while the facilitator has not yet decided; otherwise the
    /// terminal status set by the facilitator (typically `"resolved"` or
    /// `"rejected"`).
    pub status: String,
    /// The task the disagreement was about, if any.
    pub task_id: Option<String>,
    /// The facilitator's decision, if `status != "pending"`. Free-form text
    /// (e.g. "merge with the suggested fix", "reject and rework").
    pub decision: Option<String>,
    /// The facilitator's reasoning for the decision, if any.
    pub reasoning: Option<String>,
    /// Any precedent the facilitator cited (e.g. a link to a prior ADR),
    /// if any.
    pub precedent: Option<String>,
    /// When the disagreement was originally submitted (`created_at`).
    pub created_at: String,
    /// When the facilitator resolved it (`resolved_at`), if `status !=
    /// "pending"`.
    pub resolved_at: Option<String>,
}

// ============================================================================
// Handlers
// ============================================================================

/// Submit a disagreement for facilitator mediation.
///
/// Verifies the agent token (tenant-scoped), then:
///
/// 1. Looks up the task's `workflow_run_id` (if `task_id` was supplied) so we
///    know which workflow channel to announce on.
/// 2. Inserts a row into the `disagreements` table with `status = 'pending'`.
/// 3. Posts a notice on `agent_messages` on channel
///    `facilitator-<workflow_run_id>` (falling back to
///    `facilitator-task-<task_id>` when the task has no workflow run, and
///    `facilitator-machine-<machine_id>` when no task was supplied at all).
///
/// Returns `{ disagreement_id, status: "pending" }`. The caller polls
/// [`get_decision`] with the returned `disagreement_id` until `status !=
/// "pending"`.
pub async fn submit_disagreement(
    Path(machine_id): Path<String>,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SubmitDisagreementRequest>,
) -> Result<Json<SubmitDisagreementResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let disagreement_id = format!("dg_{}", ulid::Ulid::new());

    // Look up the task (if any) to resolve a workflow_run_id for the
    // facilitator channel. A missing task → 404 only when the caller
    // explicitly supplied a task_id; a NULL task_id is allowed.
    let workflow_run_id: Option<String> = match req.task_id.as_deref() {
        Some(tid) if !tid.is_empty() => {
            let conn = state
                .db
                .get()
                .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;
            let row = conn.query_row(
                "SELECT workflow_run_id FROM tasks WHERE id = ?1 AND tenant_id = ?2",
                rusqlite::params![tid, &tenant_id],
                |row| row.get::<_, Option<String>>(0),
            );
            match row {
                Ok(w) => w,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Err((
                        StatusCode::NOT_FOUND,
                        format!("Task not found: {}", tid),
                    ));
                }
                Err(e) => {
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                }
            }
        }
        _ => None,
    };

    let context_str = req
        .context
        .as_ref()
        .map(|v| v.to_string());

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    conn.execute(
        "INSERT INTO disagreements
         (id, tenant_id, task_id, machine_id, issue,
          coder_argument, reviewer_argument, context,
          decision, reasoning, precedent, status, created_at, resolved_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL,
                 'pending', datetime('now'), NULL)",
        rusqlite::params![
            &disagreement_id,
            &tenant_id,
            req.task_id,
            &machine_id,
            &req.issue,
            req.coder_argument,
            req.reviewer_argument,
            context_str,
        ],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Announce on the agent bus so a facilitator subscriber can pick it up.
    // Channel resolution order:
    //   1. facilitator-<workflow_run_id>  (preferred — scopes to the run)
    //   2. facilitator-task-<task_id>     (task-bound but no workflow run)
    //   3. facilitator-machine-<machine_id> (no task at all)
    let channel = match (workflow_run_id.as_ref(), req.task_id.as_deref()) {
        (Some(wfr), _) => format!("facilitator-{}", wfr),
        (None, Some(tid)) if !tid.is_empty() => format!("facilitator-task-{}", tid),
        _ => format!("facilitator-machine-{}", machine_id),
    };

    let announce_body = serde_json::json!({
        "disagreement_id": disagreement_id,
        "type": "disagreement",
        "task_id": req.task_id,
        "issue": req.issue,
        "tenant_id": tenant_id,
        "machine_id": machine_id,
        "channel_hint": channel,
    });
    let announce_str = announce_body.to_string();

    conn.execute(
        "INSERT INTO agent_messages (from_machine, to_machine, channel, body, created_at)
         VALUES (?1, NULL, ?2, ?3, datetime('now'))",
        rusqlite::params![&machine_id, &channel, &announce_str],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        machine = %machine_id,
        disagreement_id = %disagreement_id,
        task_id = ?req.task_id,
        channel = %channel,
        "Disagreement submitted to facilitator"
    );

    Ok(Json(SubmitDisagreementResponse {
        disagreement_id,
        status: "pending".to_string(),
    }))
}

/// Poll for the facilitator's decision on a submitted disagreement.
///
/// Reads the `disagreements` row by ID. Returns `404` if the row doesn't
/// exist or doesn't belong to the authenticated tenant. While the row's
/// `status` is still `"pending"`, the response carries `status: "pending"`
/// and `null` for `decision` / `reasoning` / `precedent` / `resolved_at`.
/// Once the facilitator updates the row to a non-pending status, those
/// fields are populated and returned.
pub async fn get_decision(
    Path((machine_id, disagreement_id)): Path<(String, String)>,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<GetDecisionResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;
    // machine_id is part of the route; we don't strictly need to enforce it
    // matches the disagreement's submitter (the tenant_id check is the
    // real authorization), but we surface it in tracing for auditability.
    let _ = &machine_id;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    let row = conn.query_row(
        "SELECT id, status, task_id, decision, reasoning, precedent,
                created_at, resolved_at
         FROM disagreements
         WHERE id = ?1 AND tenant_id = ?2",
        rusqlite::params![&disagreement_id, &tenant_id],
        |row| {
            Ok(GetDecisionResponse {
                id: row.get(0)?,
                status: row.get(1)?,
                task_id: row.get(2)?,
                decision: row.get(3)?,
                reasoning: row.get(4)?,
                precedent: row.get(5)?,
                created_at: row.get(6)?,
                resolved_at: row.get(7)?,
            })
        },
    );

    match row {
        Ok(resp) => Ok(Json(resp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err((
            StatusCode::NOT_FOUND,
            format!("Disagreement not found: {}", disagreement_id),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// ============================================================================
// Helpers (mirror routes/tasks.rs — kept private to this module so we don't
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

    // --- SubmitDisagreementRequest ----------------------------------------

    #[test]
    fn test_submit_disagreement_request_deserialize_full() {
        let json = r#"{
            "task_id": "task_01HZX9",
            "issue": "PR #42 should be merged despite failing lint",
            "coder_argument": "lint rule is overly strict; tests pass",
            "reviewer_argument": "lint failures must block all merges",
            "context": {"ci_url": "https://ci.example.com/run/42", "files": ["src/lib.rs"]}
        }"#;
        let req: SubmitDisagreementRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.task_id.as_deref(), Some("task_01HZX9"));
        assert_eq!(req.issue, "PR #42 should be merged despite failing lint");
        assert_eq!(
            req.coder_argument.as_deref(),
            Some("lint rule is overly strict; tests pass")
        );
        assert_eq!(
            req.reviewer_argument.as_deref(),
            Some("lint failures must block all merges")
        );
        assert!(req.context.is_some());
        assert_eq!(req.context.as_ref().unwrap()["ci_url"], "https://ci.example.com/run/42");
    }

    #[test]
    fn test_submit_disagreement_request_deserialize_minimal() {
        // Only `issue` is required; everything else should default to None.
        let json = r#"{ "issue": "architectural dispute over module layout" }"#;
        let req: SubmitDisagreementRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.issue, "architectural dispute over module layout");
        assert!(req.task_id.is_none());
        assert!(req.coder_argument.is_none());
        assert!(req.reviewer_argument.is_none());
        assert!(req.context.is_none());
    }

    #[test]
    fn test_submit_disagreement_request_rejects_missing_issue() {
        // `issue` is required; omitting it must fail.
        let json = r#"{ "task_id": "task_1" }"#;
        let result: Result<SubmitDisagreementRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_submit_disagreement_request_allows_null_optionals() {
        // Explicit nulls for the Option fields must deserialize as None.
        let json = r#"{
            "task_id": null,
            "issue": "x",
            "coder_argument": null,
            "reviewer_argument": null,
            "context": null
        }"#;
        let req: SubmitDisagreementRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.issue, "x");
        assert!(req.task_id.is_none());
        assert!(req.coder_argument.is_none());
        assert!(req.reviewer_argument.is_none());
        assert!(req.context.is_none());
    }

    // --- SubmitDisagreementResponse ---------------------------------------

    #[test]
    fn test_submit_disagreement_response_serialize_pending() {
        let resp = SubmitDisagreementResponse {
            disagreement_id: "dg_01HZX9Q8J7ABCDEF".to_string(),
            status: "pending".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"disagreement_id\":\"dg_01HZX9Q8J7ABCDEF\""));
        assert!(json.contains("\"status\":\"pending\""));
        // Exactly 2 keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    // --- GetDecisionResponse ----------------------------------------------

    #[test]
    fn test_get_decision_response_serialize_pending() {
        let resp = GetDecisionResponse {
            id: "dg_01".to_string(),
            status: "pending".to_string(),
            task_id: Some("task_42".to_string()),
            decision: None,
            reasoning: None,
            precedent: None,
            created_at: "2026-01-01 00:00:00".to_string(),
            resolved_at: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"pending\""));
        assert!(json.contains("\"id\":\"dg_01\""));
        assert!(json.contains("\"task_id\":\"task_42\""));
        assert!(json.contains("\"decision\":null"));
        assert!(json.contains("\"reasoning\":null"));
        assert!(json.contains("\"precedent\":null"));
        assert!(json.contains("\"resolved_at\":null"));
        assert!(json.contains("\"created_at\":\"2026-01-01 00:00:00\""));
    }

    #[test]
    fn test_get_decision_response_serialize_resolved() {
        let resp = GetDecisionResponse {
            id: "dg_02".to_string(),
            status: "resolved".to_string(),
            task_id: None,
            decision: Some("merge with suggested fix".to_string()),
            reasoning: Some("lint rule deprecated in v2".to_string()),
            precedent: Some("ADR-0008".to_string()),
            created_at: "2026-01-01 00:00:00".to_string(),
            resolved_at: Some("2026-01-01 00:05:00".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"resolved\""));
        assert!(json.contains("\"decision\":\"merge with suggested fix\""));
        assert!(json.contains("\"reasoning\":\"lint rule deprecated in v2\""));
        assert!(json.contains("\"precedent\":\"ADR-0008\""));
        assert!(json.contains("\"resolved_at\":\"2026-01-01 00:05:00\""));
        assert!(json.contains("\"task_id\":null"));
    }

    #[test]
    fn test_get_decision_response_has_eight_keys() {
        // Lock the wire shape: callers (and the orchestrator) depend on the
        // exact set of fields. This test will fail if a field is added or
        // removed without an intentional schema bump.
        let resp = GetDecisionResponse {
            id: "dg_x".to_string(),
            status: "pending".to_string(),
            task_id: None,
            decision: None,
            reasoning: None,
            precedent: None,
            created_at: "t".to_string(),
            resolved_at: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 8);
    }
}
