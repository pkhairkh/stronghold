//! Mid-session reprompt — inject new instructions into a running agent session.
//!
//! Implemented in: J4
//!
//! Three modes:
//! - `pty`: inject text into the running PTY stdin
//! - `control`: send a JSON message on the control channel (TODO N4)
//! - `task`: queue a sub-task within the existing session

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Request to inject a new instruction into a running session.
#[derive(Debug, Deserialize)]
pub struct InstructRequest {
    /// The new instruction for the agent.
    pub instruction: String,
    /// Optional context (file, line, error output, etc.).
    #[serde(default)]
    pub context: Option<serde_json::Value>,
    /// How to deliver the instruction:
    /// - `pty`: inject as text into the PTY stdin
    /// - `control`: send as JSON on the control WebSocket (TODO N4)
    /// - `task`: queue as a sub-task
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Priority hint: "low", "normal", "high".
    #[serde(default = "default_priority")]
    pub priority: String,
}

fn default_mode() -> String {
    "pty".to_string()
}

fn default_priority() -> String {
    "normal".to_string()
}

/// Response to an instruct request.
#[derive(Debug, Serialize)]
pub struct InstructResponse {
    pub status: String,
    pub mode: String,
    pub message: String,
}

/// POST /agent/:machine_id/instruct — inject a new instruction.
///
/// Verifies the agent token, then delivers the instruction via the
/// specified mode. The phone approval for the original session covers
/// the entire TTL — no additional approval is needed for reprompts.
pub async fn inject(
    Path(machine_id): Path<String>,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<InstructRequest>,
) -> Result<Json<InstructResponse>, (StatusCode, String)> {
    // Verify agent token
    let agent_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "Missing Authorization header".to_string()))?;

    let tenant_id = crate::tenants::auth::verify_agent_token(&state.db, agent_token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        machine = %machine_id,
        mode = %req.mode,
        priority = %req.priority,
        "Reprompt received"
    );

    // Write audit entry
    let instruction_snippet = if req.instruction.len() > 200 {
        &req.instruction[..200]
    } else {
        &req.instruction
    };
    let _ = crate::audit::log::entry(
        &state.db,
        &tenant_id,
        &machine_id,
        "instruct_received",
        serde_json::json!({
            "mode": req.mode,
            "priority": req.priority,
            "instruction_snippet": instruction_snippet,
        }),
        &state.audit_keys,
    );

    match req.mode.as_str() {
        "pty" => {
            // Look up the PTY stdin sender in the registry
            let registry = state.pty_registry.read().await;
            if let Some(sender) = registry.get(&machine_id) {
                // Inject the instruction as a comment + text
                let inject_text = format!(
                    "\n# Stronghold Instruction ({}): {}\n",
                    req.priority, req.instruction
                );
                sender
                    .send(inject_text.into_bytes())
                    .await
                    .map_err(|_| {
                        (
                            StatusCode::GONE,
                            "PTY session no longer active".to_string(),
                        )
                    })?;
                Ok(Json(InstructResponse {
                    status: "delivered".to_string(),
                    mode: "pty".to_string(),
                    message: "Instruction injected into PTY".to_string(),
                }))
            } else {
                Err((
                    StatusCode::NOT_FOUND,
                    "No active PTY session for this machine_id".to_string(),
                ))
            }
        }
        "task" => {
            // Create a sub-task
            let task_id = format!("task_{}", ulid::Ulid::new());
            let conn = state.db.get().map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;
            conn.execute(
                "INSERT INTO tasks (id, tenant_id, machine_id, parent_task_id, status, spec, created_at)
                 VALUES (?1, ?2, ?3, NULL, 'queued', ?4, datetime('now'))",
                rusqlite::params![
                    task_id,
                    tenant_id,
                    machine_id,
                    serde_json::to_string(&serde_json::json!({
                        "instruction": req.instruction,
                        "context": req.context,
                        "priority": req.priority,
                    }))
                    .unwrap_or_default(),
                ],
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            Ok(Json(InstructResponse {
                status: "queued".to_string(),
                mode: "task".to_string(),
                message: format!("Sub-task {} created", task_id),
            }))
        }
        "control" => {
            // Control WebSocket channel is implemented in N4
            // For now, log a warning and return accepted
            tracing::warn!(
                "Control channel mode not yet implemented (N4) — instruction accepted but not delivered via control WS"
            );
            Ok(Json(InstructResponse {
                status: "accepted".to_string(),
                mode: "control".to_string(),
                message: "Control channel delivery pending (N4)".to_string(),
            }))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("Unknown mode: {}. Use 'pty', 'control', or 'task'", req.mode),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instruct_request_deserialization() {
        let json = r#"{"instruction":"Fix the bug","mode":"pty","priority":"high"}"#;
        let req: InstructRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.instruction, "Fix the bug");
        assert_eq!(req.mode, "pty");
        assert_eq!(req.priority, "high");
        assert!(req.context.is_none());
    }

    #[test]
    fn test_instruct_request_default_mode() {
        let json = r#"{"instruction":"Do something"}"#;
        let req: InstructRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.mode, "pty");
        assert_eq!(req.priority, "normal");
    }

    #[test]
    fn test_instruct_request_with_context() {
        let json = r#"{"instruction":"Fix bug","context":{"file":"src/main.rs","line":42},"mode":"task"}"#;
        let req: InstructRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.mode, "task");
        assert!(req.context.is_some());
        let ctx = req.context.unwrap();
        assert_eq!(ctx["file"], "src/main.rs");
        assert_eq!(ctx["line"], 42);
    }

    #[test]
    fn test_instruct_response_serialization() {
        let resp = InstructResponse {
            status: "delivered".to_string(),
            mode: "pty".to_string(),
            message: "Instruction injected".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("delivered"));
        assert!(json.contains("pty"));
    }
}
