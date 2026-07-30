//! Continuous work endpoint — DYNAMIC task dispatch.
//!
//! After the blueprint pipeline is complete, the orchestrator parses
//! 06-tasks.md and dispatches unchecked coding tasks to agents.
//! Each agent gets a different task (locked, no double assignment).
//! Tasks are dispatched in dependency order (T-101 before T-102, etc.)

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

const REPO_PATH: &str = "/home/dev/work/picast";
const AUTH_TOKEN: &str = "stronghold_agent_wFQ0XDBt1blcPkfn5xI18pLpWtcLvAnMRCwtLHDmo";

/// Coding tasks parsed from 06-tasks.md — dispatched after blueprint is done.
/// Each entry: (task_id, instruction, branch, commit_message)
const CODING_TASKS: &[(&str, &str, &str, &str)] = &[
    ("T-101", "Bootstrap the Rust workspace skeleton. Create workspace Cargo.toml with 13 crates as empty crates with stub lib.rs/main.rs. Add clippy::unwrap_used lint config.", "feat/t-101-workspace-skeleton", "feat: bootstrap workspace skeleton (T-101)"),
    ("T-102", "Implement bogdan-config crate: TOML parser, env-var overlay, unknown-field rejection, security-field-restart invariant.", "feat/t-102-config-crate", "feat: implement bogdan-config crate (T-102)"),
    ("T-103", "Implement bogdan-tor crate: daemon supervisor. Start C Tor daemon via systemd, monitor via control port, restart on crash.", "feat/t-103-tor-supervisor", "feat: implement tor daemon supervisor (T-103)"),
    ("T-104", "Implement IsolateSOCKSAuth username derivation in bogdan-tor: SHA-256 of host, first 16 hex chars. Deterministic and collision-resistant.", "feat/t-104-socks-auth", "feat: implement IsolateSOCKSAuth username derivation (T-104)"),
    ("T-105", "Create config/torrc and config/iptables.rules. Hardened torrc (AvoidDiskWrites, SafeLogging, CookieAuthentication, IsolateSOCKSAuth). iptables rules dropping all non-Tor outbound.", "feat/t-105-torrc-iptables", "feat: add hardened torrc + iptables rules (T-105)"),
    ("T-106", "Create scripts/verify-network-isolation.sh. Runs tcpdump during a 60-second mock cast and asserts zero non-Tor packets.", "feat/t-106-verify-isolation", "feat: add network isolation verification script (T-106)"),
    ("T-107", "Implement bogdan-resolver crate: in-tree YouTube resolver. ~150-line Rust resolver using reqwest over socks5h://, returns ResolvedMedia within 10s.", "feat/t-107-youtube-resolver", "feat: implement YouTube resolver (T-107)"),
    ("T-201", "Implement bogdan-display crate: DRM master + atomic modesetting. Open /dev/dri/card0, acquire DRM master, program CRTC via drmModeAtomicCommit for plane 0.", "feat/t-201-display-drm", "feat: implement DRM display crate (T-201)"),
    ("T-202", "Implement bogdan-playback crate: GStreamer pipeline construction. Build appsrc → queue2 → parsebin → v4l2h264dec → v4l2convert → kmssink for video.", "feat/t-202-playback-pipeline", "feat: implement GStreamer playback pipeline (T-202)"),
    ("T-301", "Implement bogdan-session crate: state machine. Implement Session, CastCommand, SessionState, ErrorCode. Single-threaded behind Arc<Mutex<Session>>.", "feat/t-301-session-state", "feat: implement session state machine (T-301)"),
    ("T-302", "Implement HTTP REST facade: POST /api/cast, /stop, /pause, /resume, /seek, GET /api/status. Translate each into CastCommand. CORS *.", "feat/t-302-http-facade", "feat: implement HTTP REST facade (T-302)"),
    ("T-303", "Implement WebSocket facade on :8586/events. Push state_changed, buffer_update, circuit_rotated, thermal_throttled, error events. 1024-entry ring buffer.", "feat/t-303-ws-facade", "feat: implement WebSocket facade (T-303)"),
    ("T-304", "Implement DLNA facade: gmediarender subprocess management. Spawn gmediarender, advertise via SSDP, accept SetAVTransportURI.", "feat/t-304-dlna-facade", "feat: implement DLNA facade (T-304)"),
    ("T-306", "Implement bogdan-server main binary: startup orchestration. Parse config, spawn tor supervisor, three facades, single Session. Expose /api/status.", "feat/t-306-server-main", "feat: implement server main binary (T-306)"),
    ("T-307", "Create conformance suites: HTTP, WebSocket, DLNA. tests/conformance/{http,ws,dlna}/ with pytest + curl + wscat.", "feat/t-307-conformance", "feat: add conformance test suites (T-307)"),
];

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
    let locked_task: Option<(String, String)> = {
        let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        let task_row: Option<(String, String)> = conn.query_row(
            "SELECT id, spec FROM tasks WHERE tenant_id = ?1 AND status = 'queued' ORDER BY created_at ASC LIMIT 1",
            rusqlite::params![tenant_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).ok();

        if let Some((ref tid, ref spec_str)) = task_row {
            let rows = conn.execute(
                "UPDATE tasks SET status = 'running', machine_id = ?1 WHERE id = ?2 AND status = 'queued'",
                rusqlite::params![machine_id, tid],
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            if rows > 0 {
                tracing::info!(tenant = %tenant_id, machine = %machine_id, task_id = %tid, "Task locked by next_work");
                Some((tid.clone(), spec_str.clone()))
            } else {
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
        let coding_task_id = spec.get("coding_task_id").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // CC1: Post task_started on message bus
        post_message(&state, &machine_id, &tenant_id, &task_id, &phase, "task_started").await;

        let branch = if coding_task_id.is_empty() {
            format!("docs/blueprint-{}", phase)
        } else {
            format!("feat/{}", coding_task_id.to_lowercase())
        };
        let commit_msg = if coding_task_id.is_empty() {
            format!("docs: {} (blueprint pipeline)", phase)
        } else {
            format!("feat: implement {} ({})", coding_task_id, phase)
        };

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

    // 3. No queued task — find the next work to do.
    // First check if blueprint phases are all done.
    let completed_phases: Vec<String> = {
        let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT json_extract(spec, '$.blueprint_phase') as phase
             FROM tasks WHERE tenant_id = ?1 AND status IN ('completed', 'running')
             AND json_extract(spec, '$.blueprint_phase') IS NOT NULL",
        ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt.query_map(rusqlite::params![tenant_id], |row| row.get::<_, String>(0))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let blueprint_phases = ["problem_catalog", "rough_draft", "adrs", "fine_draft", "spec", "tasks", "progress"];
    let blueprint_done = blueprint_phases.iter().all(|p| completed_phases.contains(&p.to_string()));

    if !blueprint_done {
        // Still in blueprint phase — find next blueprint task
        let blueprint_pipeline: &[(&str, &str)] = &[
            ("problem_catalog", "Create docs/blueprint/01-problem-catalog.md. Read the repo, identify all problems, stakeholders, constraints."),
            ("rough_draft", "Create docs/blueprint/02-rough-draft.md. For EACH problem, propose a solution + alternative + risk."),
            ("adrs", "Create docs/blueprint/03-adrs/ — one ADR file per major decision."),
            ("fine_draft", "Create docs/blueprint/04-fine-draft.md. Architecture: components, data model, security, test strategy."),
            ("spec", "Create docs/blueprint/05-spec.md. Requirements with acceptance criteria."),
            ("tasks", "Create docs/blueprint/06-tasks.md. Task breakdown with checkboxes, roles, estimates, dependencies."),
            ("progress", "Create docs/blueprint/07-progress.md. Living progress doc."),
        ];

        let next = blueprint_pipeline.iter()
            .find(|(phase, _)| !completed_phases.contains(&phase.to_string()));

        if let Some((phase, instruction)) = next {
            let task_id = format!("task_{}", ulid::Ulid::new());
            let spec = serde_json::json!({
                "instruction": instruction,
                "image": "localhost:30500/stronghold/rust-stable:latest",
                "ttl_secs": 3600,
                "blueprint_phase": phase,
                "machine_id": machine_id,
            });

            let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
            conn.execute(
                "INSERT INTO tasks (id, tenant_id, machine_id, spec, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', datetime('now'))",
                rusqlite::params![task_id, tenant_id, machine_id, spec.to_string()],
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            post_message(&state, &machine_id, &tenant_id, &task_id, phase, "task_started").await;

            return Ok(Json(NextWorkResponse {
                status: "work".to_string(),
                task_id,
                instruction: instruction.to_string(),
                machine_id,
                repo_path: REPO_PATH.to_string(),
                branch: format!("docs/blueprint-{}", phase),
                commit_message: format!("docs: add {} (blueprint pipeline)", phase),
                phase: phase.to_string(),
            }).into_response());
        }
    }

    // 4. Blueprint done — dispatch CODING TASKS dynamically.
    // Find coding tasks that haven't been dispatched yet.
    let dispatched_task_ids: Vec<String> = {
        let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT json_extract(spec, '$.coding_task_id') as tid
             FROM tasks WHERE tenant_id = ?1
             AND json_extract(spec, '$.coding_task_id') IS NOT NULL
             AND json_extract(spec, '$.coding_task_id') != ''",
        ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt.query_map(rusqlite::params![tenant_id], |row| row.get::<_, String>(0))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let next_coding_task = CODING_TASKS.iter()
        .find(|(tid, _, _, _)| !dispatched_task_ids.contains(&tid.to_string()));

    match next_coding_task {
        Some((coding_task_id, instruction, branch, commit_msg)) => {
            let task_id = format!("task_{}", ulid::Ulid::new());
            let spec = serde_json::json!({
                "instruction": instruction,
                "image": "localhost:30500/stronghold/rust-stable:latest",
                "ttl_secs": 7200,
                "blueprint_phase": "coding",
                "coding_task_id": coding_task_id,
                "machine_id": machine_id,
            });

            let conn = state.db.get().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
            conn.execute(
                "INSERT INTO tasks (id, tenant_id, machine_id, spec, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', datetime('now'))",
                rusqlite::params![task_id, tenant_id, machine_id, spec.to_string()],
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            tracing::info!(tenant = %tenant_id, machine = %machine_id, task_id = %task_id, coding_task = %coding_task_id, "Orchestrator: dispatched coding task");

            post_message(&state, &machine_id, &tenant_id, &task_id, "coding", "task_started").await;

            Ok(Json(NextWorkResponse {
                status: "work".to_string(),
                task_id,
                instruction: instruction.to_string(),
                machine_id,
                repo_path: REPO_PATH.to_string(),
                branch: branch.to_string(),
                commit_message: commit_msg.to_string(),
                phase: format!("coding-{}", coding_task_id),
            }).into_response())
        }
        None => {
            // All coding tasks dispatched — check for code review tasks
            // or return idle
            Ok(Json(NextWorkResponse {
                status: "idle".to_string(),
                task_id: String::new(),
                instruction: "All tasks dispatched. Review existing code for bugs. If you find issues, fix them, commit, and push.".to_string(),
                machine_id,
                repo_path: REPO_PATH.to_string(),
                branch: String::new(),
                commit_message: String::new(),
                phase: String::new(),
            }).into_response())
        }
    }
}

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
