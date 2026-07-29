//! Agent protocol endpoints.
//!
//! The agent protocol decouples machine lifetime from agent connection.
//! A machine has a TTL; the agent attaches and detaches freely without
//! killing it.
//!
//! Endpoints:
//! - `POST /agent/order` — Request a new machine
//! - `POST /agent/resume` — Reattach to an existing machine
//! - `POST /agent/release` — Kill the machine early
//! - `POST /agent/extend` — Request more time (triggers phone approval)
//! - `GET  /agent/health` — Health check

use crate::routes::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

/// Request a new machine (triggers phone approval).
#[derive(Debug, Deserialize)]
pub struct OrderRequest {
    /// OCI image to use, e.g. "stronghold/rust-nightly:2026.07"
    pub image: String,
    /// Session TTL in seconds
    pub ttl_secs: u64,
    /// Human-readable reason for the session
    pub reason: String,
    /// Compute requirements
    #[serde(default)]
    pub compute: ComputeRequest,
    /// Ephemeral volume mounts
    #[serde(default)]
    pub ephemeral_volumes: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ComputeRequest {
    #[serde(default)]
    pub cpu: Option<u32>,
    #[serde(default)]
    pub memory_gb: Option<u32>,
    #[serde(default)]
    pub dedicated: Option<bool>,
    #[serde(default)]
    pub gpu: Option<bool>,
}

/// Response to a successful ORDER (after phone approval).
#[derive(Debug, Serialize)]
pub struct OrderResponse {
    pub machine_id: String,
    pub connect_token: String,
    pub expires_at: String,
    pub worker: String,
    pub worker_sev_snp_attested: bool,
    pub pty_endpoint: String,
    pub audit_stream: String,
}

/// Request a new machine.
///
/// This creates a pending session and pushes the tenant's phones via ntfy.
/// The HTTP response is held open (long-poll) until:
/// - The tenant approves → 200 with `OrderResponse`
/// - The tenant denies → 403
/// - Timeout (60s) → 408
pub async fn order(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<OrderRequest>,
) -> Result<Json<OrderResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    tracing::info!(
        tenant = %tenant_id,
        image = %req.image,
        ttl = req.ttl_secs,
        "Agent ORDER received"
    );

    // Create pending session
    let session_id = crate::sessions::manager::create_pending(&state.db, &tenant_id, &req)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Push to phones via ntfy
    crate::push::ntfy::push_approval_request(&tenant_id, &session_id, &req)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    // Long-poll for decision (60s timeout)
    let decision = crate::sessions::manager::wait_for_decision(&state.db, &session_id, 60)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match decision {
        crate::sessions::manager::Decision::Approved => {
            let resp =
                crate::sessions::manager::finalize_session(&state, &tenant_id, &session_id, &req)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            Ok(Json(resp))
        }
        crate::sessions::manager::Decision::Denied => Err((
            StatusCode::FORBIDDEN,
            "Session denied by tenant".to_string(),
        )),
        crate::sessions::manager::Decision::Timeout => Err((
            StatusCode::REQUEST_TIMEOUT,
            "Approval timed out".to_string(),
        )),
    }
}

/// Reattach to an existing machine.
#[derive(Debug, Deserialize)]
pub struct ResumeRequest {
    pub machine_id: String,
}

pub async fn resume(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ResumeRequest>,
) -> Result<Json<OrderResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    tracing::info!(
        tenant = %tenant_id,
        machine = %req.machine_id,
        "Agent RESUME received"
    );

    crate::sessions::manager::resume_session(&state, &tenant_id, &req.machine_id)
        .map_err(|e| {
            let code = match e.downcast_ref::<crate::sessions::manager::SessionError>() {
                Some(crate::sessions::manager::SessionError::NotFound) => StatusCode::NOT_FOUND,
                Some(crate::sessions::manager::SessionError::Expired) => StatusCode::GONE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (code, e.to_string())
        })
        .map(Json)
}

/// Release (kill) a machine early.
#[derive(Debug, Deserialize)]
pub struct ReleaseRequest {
    pub machine_id: String,
}

pub async fn release(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ReleaseRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    tracing::info!(
        tenant = %tenant_id,
        machine = %req.machine_id,
        "Agent RELEASE received"
    );

    crate::sessions::manager::release_session(&state, &tenant_id, &req.machine_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::OK)
}

/// Extend a session (triggers phone approval).
#[derive(Debug, Deserialize)]
pub struct ExtendRequest {
    pub machine_id: String,
    pub additional_secs: u64,
}

pub async fn extend(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ExtendRequest>,
) -> Result<Json<OrderResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    tracing::info!(
        tenant = %tenant_id,
        machine = %req.machine_id,
        additional = req.additional_secs,
        "Agent EXTEND received"
    );

    // EXTEND triggers a new phone approval
    let session_id = crate::sessions::manager::create_extend_request(&state.db, &tenant_id, &req)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    crate::push::ntfy::push_extend_request(&tenant_id, &session_id, &req)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let decision = crate::sessions::manager::wait_for_decision(&state.db, &session_id, 60)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match decision {
        crate::sessions::manager::Decision::Approved => {
            let resp =
                crate::sessions::manager::finalize_extend(&state, &tenant_id, &session_id, &req)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            Ok(Json(resp))
        }
        crate::sessions::manager::Decision::Denied => {
            Err((StatusCode::FORBIDDEN, "Extension denied".to_string()))
        }
        crate::sessions::manager::Decision::Timeout => Err((
            StatusCode::REQUEST_TIMEOUT,
            "Approval timed out".to_string(),
        )),
    }
}

/// Health check endpoint.
pub async fn health() -> StatusCode {
    StatusCode::OK
}

// --- Helpers ---

fn extract_token(headers: &axum::http::HeaderMap) -> Result<String, (StatusCode, String)> {
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

fn authenticate_agent(state: &AppState, token: &str) -> Result<String, (StatusCode, String)> {
    crate::tenants::auth::verify_agent_token(&state.db, token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))
}
