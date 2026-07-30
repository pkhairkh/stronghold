//! Continuous work endpoint — keeps agents alive by queuing the next task.
//!
//! The core problem: free-tier agents are stateless and will emit EOS
//! (end of sequence) when they think they're "done." To prevent this,
//! the orchestrator must ALWAYS have more work ready. When an agent
//! calls `stronghold_result`, the orchestrator immediately creates the
//! next task in the blueprint pipeline. The agent then polls
//! `GET /agent/:machine_id/next-work` to get it.
//!
//! This creates a continuous loop:
//!   1. Agent gets task (via next-work endpoint)
//!   2. Agent does task
//!   3. Agent calls stronghold_result
//!   4. Orchestrator auto-creates the next task
//!   5. Agent polls next-work → gets the new task → goto 1
//!
//! The agent NEVER stops. It NEVER emits EOS. It loops forever until
//! the machine TTL expires or the orchestrator runs out of work.

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

/// The blueprint pipeline — ordered list of tasks the orchestrator
/// auto-creates for the picast project. When a task completes, the
/// orchestrator finds the next one in this list that hasn't been done.
const BLUEPRINT_PIPELINE: &[(&str, &str)] = &[
    ("problem_catalog", "Create docs/blueprint/01-problem-catalog.md. Read the repo, identify all problems boGDan solves, stakeholders, constraints. Follow the blueprint document convention with YAML front-matter + [[P-NNN]] IDs."),
    ("rough_draft", "Create docs/blueprint/02-rough-draft.md. Read 01-problem-catalog.md. For EACH problem [[P-NNN]], propose a solution with an alternative considered and a risk. Address all 12 problems."),
    ("adrs", "Create docs/blueprint/03-adrs/ADR-001-through-ADR-005. One ADR file per major architectural decision. Each ADR must have: Context, Decision, Consequences (positive + negative), Alternatives. Cover: Tor routing, V4L2 decode pipeline, protocol surface, DRM-free approach, content resolution."),
    ("fine_draft", "Create docs/blueprint/04-fine-draft.md. Read all prior docs. Describe the architecture: components [[C-NNN]], data model, security considerations, test strategy. Reference ADRs."),
    ("spec", "Create docs/blueprint/05-spec.md. Read all prior docs. Define requirements [[R-NNN]] with acceptance criteria (checkboxes). Every problem [[P-NNN]] must be addressed by at least one requirement [[R-NNN]]."),
    ("tasks", "Create docs/blueprint/06-tasks.md. Read the spec. Break down into tasks [[T-NNN]] with | role:coder | est:4h | dep:T-001 | implements:R-006 |. Every requirement [[R-NNN]] must be implemented by at least one task [[T-NNN]]."),
    ("progress", "Create docs/blueprint/07-progress.md. Read the tasks. Create a living progress document with status, task checkboxes, blockers, next steps. This document will be updated continuously."),
];

/// `GET /agent/:machine_id/next-work` — returns the next task for this machine.
///
/// The orchestrator looks at the machine's tenant, finds all tasks for that
/// tenant, determines which blueprint phase is next, and either returns an
/// existing queued task or creates a new one.
///
/// Returns 200 with a task JSON if work is available, 204 if no work.
pub async fn next_work(
    State(state): State<AppState>,
    Path(machine_id): Path<String>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;

    // 1. Find the tenant for this machine
    let tenant_id: String = {
        let conn = state
            .db
            .get()
            .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        conn.query_row(
            "SELECT tenant_id FROM machines WHERE id = ?1 AND status = 'active'",
            rusqlite::params![machine_id],
            |row| row.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                (StatusCode::NOT_FOUND, format!("Machine not found: {}", machine_id))
            }
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?
    };

    // 2. Check for existing queued tasks for this tenant
    let existing_task: Option<(String, String)> = {
        let conn = state
            .db
            .get()
            .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        conn.query_row(
            "SELECT id, spec FROM tasks WHERE tenant_id = ?1 AND status = 'queued' ORDER BY created_at ASC LIMIT 1",
            rusqlite::params![tenant_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok()
    };

    if let Some((task_id, spec_str)) = existing_task {
        // Found a queued task — return it
        let spec: serde_json::Value = serde_json::from_str(&spec_str).unwrap_or_default();
        return Ok(Json(NextWorkResponse {
            status: "work".to_string(),
            task_id,
            instruction: spec
                .get("instruction")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            machine_id: machine_id.clone(),
        })
        .into_response());
    }

    // 3. No queued task — determine which blueprint phase is next
    // Check which documents already have completed tasks
    let completed_phases: Vec<String> = {
        let conn = state
            .db
            .get()
            .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT json_extract(spec, '$.blueprint_phase') as phase
                 FROM tasks
                 WHERE tenant_id = ?1 AND status = 'completed'
                 AND json_extract(spec, '$.blueprint_phase') IS NOT NULL",
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![tenant_id], |row| row.get::<_, String>(0))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    // Find the next phase in the pipeline
    let next_phase = BLUEPRINT_PIPELINE
        .iter()
        .find(|(phase, _)| !completed_phases.contains(&phase.to_string()));

    match next_phase {
        Some((phase, instruction)) => {
            // Create the task
            let task_id = format!("task_{}", ulid::Ulid::new());
            let spec = serde_json::json!({
                "instruction": instruction,
                "image": "localhost:30500/stronghold/rust-stable:latest",
                "ttl_secs": 3600,
                "blueprint_phase": phase,
                "machine_id": machine_id,
            });

            {
                let conn = state
                    .db
                    .get()
                    .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
                conn.execute(
                    "INSERT INTO tasks (id, tenant_id, machine_id, spec, status, created_at)
                     VALUES (?1, ?2, ?3, ?4, 'queued', datetime('now'))",
                    rusqlite::params![task_id, tenant_id, machine_id, spec.to_string()],
                )
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            }

            tracing::info!(
                tenant = %tenant_id,
                machine = %machine_id,
                task_id = %task_id,
                phase = %phase,
                "Orchestrator: auto-created next blueprint task"
            );

            Ok(Json(NextWorkResponse {
                status: "work".to_string(),
                task_id,
                instruction: instruction.to_string(),
                machine_id,
            })
            .into_response())
        }
        None => {
            // All blueprint phases complete — return "no work" but keep the
            // agent alive by suggesting it review/improve existing docs
            Ok(Json(NextWorkResponse {
                status: "idle".to_string(),
                task_id: String::new(),
                instruction: "All blueprint phases complete. Review the existing documents for quality. If you find issues, fix them. Otherwise, wait for new tasks.".to_string(),
                machine_id,
            })
            .into_response())
        }
    }
}

#[derive(Debug, Serialize)]
pub struct NextWorkResponse {
    pub status: String,      // "work" or "idle"
    pub task_id: String,     // empty if idle
    pub instruction: String,
    pub machine_id: String,
}
