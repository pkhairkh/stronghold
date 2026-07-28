//! HTTP/WebSocket route handlers for the Stronghold gateway.
//!
//! Module tree:
//! - `agent` — Agent protocol endpoints (ORDER/RESUME/RELEASE/EXTEND)
//! - `phone` — Phone-side endpoints (ntfy callbacks, WebAuthn verify)
//! - `admin` — Tenant management and admin endpoints
//! - `pty` — WebSocket PTY proxy
//! - `attestation` — SEV-SNP attestation report endpoint

pub mod agent;
pub mod phone;
pub mod admin;
pub mod pty;
pub mod attestation;

use axum::Router;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use crate::crypto::hybrid_sig::AuditKeys;
use crate::crypto::hybrid_kem::PushKeys;

/// Shared application state passed to all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: Pool<SqliteConnectionManager>,
    pub audit_keys: AuditKeys,
    pub push_keys: PushKeys,
}

/// Build the main axum router with all routes.
pub fn build_router(
    db_pool: Pool<SqliteConnectionManager>,
    audit_keys: AuditKeys,
    push_keys: PushKeys,
) -> Router {
    let state = AppState {
        db: db_pool,
        audit_keys,
        push_keys,
    };

    Router::new()
        // Agent protocol
        .route("/agent/order", axum::routing::post(agent::order))
        .route("/agent/resume", axum::routing::post(agent::resume))
        .route("/agent/release", axum::routing::post(agent::release))
        .route("/agent/extend", axum::routing::post(agent::extend))
        .route("/agent/health", axum::routing::get(agent::health))
        // WebSocket PTY
        .route("/agent/:machine_id/pty", axum::routing::get(pty::handle_pty_ws))
        .route("/agent/:machine_id/audit", axum::routing::get(pty::handle_audit_ws))
        // Phone-side
        .route("/phone/pending", axum::routing::get(phone::pending_sse))
        .route("/phone/decide", axum::routing::post(phone::decide))
        .route("/phone/revoke", axum::routing::post(phone::revoke))
        .route("/phone/enroll", axum::routing::post(phone::enroll))
        // Admin
        .route("/admin/tenant", axum::routing::post(admin::create_tenant))
        .route("/admin/tenant/:id", axum::routing::get(admin::get_tenant))
        // Setup (one-time enrollment)
        .route("/setup", axum::routing::get(phone::setup_page))
        // Attestation
        .route("/attestation", axum::routing::get(attestation::get_report))
        // Static files (phone enrollment PWA)
        .nest_service("/static", tower_http::services::ServeDir::new("../phone"))
        .with_state(state)
}
