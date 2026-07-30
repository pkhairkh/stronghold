//! Continuous work endpoint — keeps agents alive by queuing the next task.
//!
//! CA1: Task locking — atomically UPDATE queued→running + set machine_id
//! CA2: Per-agent pod — if machine doesn't exist, schedule a new pod
//! CC1: Auto-post task_started on message bus
//! CC3: Skip phases other agents are already working on
//! CD1: Include repo_path + branch + commit_message in response

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

const BLUEPRINT_PIPELINE: &[(&str, &str, &str, &str)] = &[
    ("problem_catalog", "Create docs/blueprint/01-problem-catalog.md. Read the repo, identify all problems boGDan solves, stakeholders, constraints. Follow the blueprint document convention with YAML front-matter + [[P-NNN]] IDs.", "docs/blueprint/01-problem-catalog.md", "docs: add problem catalog (blueprint pipeline)"),
    ("rough_draft", "Create docs/blueprint/02-rough-draft.md. Read 01-problem-catalog.md. For EACH problem [[P-NNN]], propose a solution with an alternative considered and a risk. Address all 12 problems.", "docs/blueprint/02-rough-draft.md", "docs: add rough draft (blueprint pipeline)"),
    ("adrs", "Create docs/blueprint/03-adrs/ADR-001-through-ADR-005. One ADR file per major architectural decision. Each ADR must have: Context, Decision, Consequences (positive + negative), Alternatives. Cover: Tor routing, V4L2 decode pipeline, protocol surface, DRM-free approach, content resolution.", "docs/blueprint/03-adrs/", "docs: add ADRs (blueprint pipeline)"),
    ("fine_draft", "Create docs/blueprint/04-fine-draft.md. Read all prior docs. Describe the architecture: components [[C-NNN]], data model, security considerations, test strategy. Reference ADRs.", "docs/blueprint/04-fine-draft.md", "docs: add fine draft (blueprint pipeline)"),
    ("spec", "Create docs/blueprint/05-spec.md. Read all prior docs. Define requirements [[R-NNN]] with acceptance criteria (checkboxes). Every problem [[P-NNN]] must be addressed by at least one requirement [[R-NNN]].", "docs/blueprint/05-spec.md", "docs: add spec (blueprint pipeline)"),
    ("tasks", "Create docs/blueprint/06-tasks.md. Read the spec. Break down into tasks [[T-NNN]] with | role:coder | est:4h | dep:T-001 | implements:R-006 |. Every requirement [[R-NNN]] must be implemented by at least one task [[T-NNN]].", "docs/blueprint/06-tasks.md", "docs: add tasks (blueprint pipeline)"),
    ("progress", "Create docs/blueprint/07-progress.md. Read the tasks. Create a living progress document with status, task checkboxes, blockers, next steps. This document will be updated continuously.", "docs/blueprint/07-progress.md", "docs: add progress (blueprint pipeline)"),
];

const REPO_PATH: &str = "/home/dev/work/picast";

pub async fn next_work(
    State(state): State<AppState>,
    Path(machine_id): Path<String>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;

    // 1. Find the tenant for this machine
    let tenant_id: String = {
        let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        conn.query_row(
            "SELECT tenant_id FROM machines WHERE id = ?1 AND status = 'active'",
            rusqlite::params![machine_id],
            |row| row.get(0),
        ).map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => (StatusCode::NOT_FOUND, format!("Machine not found: {}", machine_id)),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?
    };

    // 2. Try to lock an existing queued task ATOMICALLY
    //    UPDATE ... WHERE status='queued' prevents races — only one agent
    //    can claim each task.
    let locked_task: Option<(String, String)> = {
        let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        // Try to lock the oldest queued task for this tenant
        let task_row: Option<(String, String)> = conn.query_row(
            "SELECT id, spec FROM tasks WHERE tenant_id = ?1 AND status = 'queued' ORDER BY created_at ASC LIMIT 1",
            rusqlite::params![tenant_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).ok();

        if let Some((ref tid, ref spec_str)) = task_row {
            // Atomic lock: only succeeds if status is still 'queued'
            let rows = conn.execute(
                "UPDATE tasks SET status = 'running', machine_id = ?1 WHERE id = ?2 AND status = 'queued'",
                rusqlite::params![machine_id, tid],
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            if rows > 0 {
                // Successfully locked
                tracing::info!(tenant = %tenant_id, machine = %machine_id, task_id = %tid, "Task locked by next_work");
                Some((tid.clone(), spec_str.clone()))
            } else {
                // Another agent grabbed it — try next (recurse not possible, return None)
                None
            }
        } else {
            None
        }
    };

    if let Some((task_id, spec_str)) = locked_task {
        let spec: serde_json::Value = serde_json::from_str(&spec_str).unwrap_or_default();
        let phase = spec.get("blueprint_phase").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let instruction = spec.get("instruction").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // CC1: Post task_started on message bus
        post_message(&state, &machine_id, &tenant_id, &task_id, &phase, "task_started").await;

        // CD1: Include git info
        let branch = format!("docs/blueprint-{}", phase);
        let commit_msg = BLUEPRINT_PIPELINE.iter()
            .find(|(p, _, _, _)| *p == phase)
            .map(|(_, _, _, cm)| cm.to_string())
            .unwrap_or_else(|| format!("docs: add {} (blueprint pipeline)", phase));

        return Ok(Json(NextWorkResponse {
            status: "work".to_string(),
            task_id,
            instruction,
            machine_id: machine_id.clone(),
            repo_path: REPO_PATH.to_string(),
            branch,
            commit_message: commit_msg,
            phase,
        }).into_response());
    }

    // 3. No locked queued task — find the next blueprint phase
    let completed_phases: Vec<String> = {
        let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT json_extract(spec, '$.blueprint_phase') as phase
             FROM tasks WHERE tenant_id = ?1 AND status IN ('completed', 'running')
             AND json_extract(spec, '$.blueprint_phase') IS NOT NULL",
        ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // CC3: Include 'running' so we skip phases other agents are working on
        let rows = stmt.query_map(rusqlite::params![tenant_id], |row| row.get::<_, String>(0))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let next_phase = BLUEPRINT_PIPELINE.iter()
        .find(|(phase, _, _, _)| !completed_phases.contains(&phase.to_string()));

    match next_phase {
        Some((phase, instruction, doc_path, commit_msg)) => {
            let task_id = format!("task_{}", ulid::Ulid::new());
            let spec = serde_json::json!({
                "instruction": instruction,
                "image": "localhost:30500/stronghold/rust-stable:latest",
                "ttl_secs": 3600,
                "blueprint_phase": phase,
                "machine_id": machine_id,
            });

            // Create + immediately lock (status='running')
            let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
            conn.execute(
                "INSERT INTO tasks (id, tenant_id, machine_id, spec, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', datetime('now'))",
                rusqlite::params![task_id, tenant_id, machine_id, spec.to_string()],
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            tracing::info!(tenant = %tenant_id, machine = %machine_id, task_id = %task_id, phase = %phase, "Orchestrator: created + locked next blueprint task");

            // CC1: Post task_started on message bus
            post_message(&state, &machine_id, &tenant_id, &task_id, phase, "task_started").await;

            Ok(Json(NextWorkResponse {
                status: "work".to_string(),
                task_id,
                instruction: instruction.to_string(),
                machine_id,
                repo_path: REPO_PATH.to_string(),
                branch: format!("docs/blueprint-{}", phase),
                commit_message: commit_msg.to_string(),
                phase: phase.to_string(),
            }).into_response())
        }
        None => {
            Ok(Json(NextWorkResponse {
                status: "idle".to_string(),
                task_id: String::new(),
                instruction: "All blueprint phases complete. Review the existing documents for quality. If you find issues, fix them and commit. Otherwise, wait for new tasks.".to_string(),
                machine_id,
                repo_path: REPO_PATH.to_string(),
                branch: String::new(),
                commit_message: String::new(),
                phase: String::new(),
            }).into_response())
        }
    }
}

/// Post a message on the project message bus.
async fn post_message(
    state: &AppState,
    machine_id: &str,
    tenant_id: &str,
    task_id: &str,
    phase: &str,
    msg_type: &str,
) {
    let channel = format!("project-{}", tenant_id);
    let body = serde_json::json!({
        "role": "agent",
        "type": msg_type,
        "task_id": task_id,
        "phase": phase,
        "machine_id": machine_id,
    });

    let conn = match state.db.get() {
        Ok(c) => c,
        Err(_) => return,
    };

    let _ = conn.execute(
        "INSERT INTO agent_messages (from_machine, to_machine, channel, body, created_at)
         VALUES (?1, NULL, ?2, ?3, datetime('now'))",
        rusqlite::params![machine_id, channel, body.to_string()],
    );

    tracing::info!(machine = %machine_id, channel = %channel, msg_type = %msg_type, phase = %phase, "Auto-posted message on bus");
}

#[derive(Debug, Serialize)]
pub struct NextWorkResponse {
    pub status: String,
    pub task_id: String,
    pub instruction: String,
    pub machine_id: String,
    pub repo_path: String,
    pub branch: String,
    pub commit_message: String,
    pub phase: String,
}
