//! HTTP/WebSocket route handlers for the Stronghold gateway.
//!
//! Module tree:
//! - `agent` — Agent protocol endpoints (ORDER/RESUME/RELEASE/EXTEND)
//! - `phone` — Phone-side endpoints (ntfy callbacks, WebAuthn verify)
//! - `admin` — Tenant management and admin endpoints
//! - `pty` — WebSocket PTY proxy
//! - `attestation` — SEV-SNP attestation report endpoint

pub mod admin;
pub mod agent;
pub mod attestation;
pub mod credentials;
pub mod exec;
pub mod git;
pub mod instruct;
pub mod messages;
pub mod metrics;
pub mod phone;
pub mod pty;
pub mod tasks;
pub mod workflows;

use crate::crypto::hybrid_kem::PushKeys;
use crate::crypto::hybrid_sig::AuditKeys;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

/// Maximum number of concurrent in-flight HTTP requests before the gateway
/// starts returning `503 Service Unavailable`.
///
/// This is a coarse **global** limit that protects the gateway from resource
/// exhaustion under burst traffic. Per-token / per-tenant rate limiting is
/// **TODO** — see `docs/adr/` for the planned token-bucket design.
const MAX_CONCURRENT_REQUESTS: usize = 100;

/// Shared application state passed to all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: Pool<SqliteConnectionManager>,
    pub audit_keys: AuditKeys,
    pub push_keys: PushKeys,
    /// Registry of active PTY sessions: machine_id → stdin sender.
    /// Used by the mid-session reprompt endpoint to inject text into running sessions.
    pub pty_registry: Arc<tokio::sync::RwLock<std::collections::HashMap<String, tokio::sync::mpsc::Sender<Vec<u8>>>>>,
}

/// Concurrency-limiting middleware.
///
/// Uses a `tokio::sync::Semaphore` to cap the number of simultaneously
/// in-flight requests at [`MAX_CONCURRENT_REQUESTS`]. When the semaphore is
/// exhausted the middleware immediately returns `503 Service Unavailable`
/// (matching the behaviour of `tower::limit::ConcurrencyLimitLayer`).
///
/// The `OwnedSemaphorePermit` is held for the lifetime of the request and
/// released when the response is produced, keeping the count accurate.
async fn concurrency_limit(
    State(semaphore): State<Arc<Semaphore>>,
    request: Request,
    next: Next,
) -> Response {
    match semaphore.clone().try_acquire_owned() {
        Ok(_permit) => next.run(request).await,
        Err(_) => {
            tracing::warn!(
                max_concurrent = MAX_CONCURRENT_REQUESTS,
                "concurrency limit reached — returning 503"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "server at capacity — retry later",
            )
                .into_response()
        }
    }
}

/// Build the main axum router with all routes.
pub fn build_router(
    db_pool: Pool<SqliteConnectionManager>,
    audit_keys: AuditKeys,
    push_keys: PushKeys,
) -> (Router, AppState) {
    let state = AppState {
        db: db_pool,
        audit_keys,
        push_keys,
        pty_registry: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    };

    // Global concurrency limiter — shared across all routes.
    let concurrency_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));

    let router = Router::new()
        // Agent protocol
        .route("/agent/order", axum::routing::post(agent::order))
        .route("/agent/resume", axum::routing::post(agent::resume))
        .route("/agent/release", axum::routing::post(agent::release))
        .route("/agent/extend", axum::routing::post(agent::extend))
        .route("/agent/health", axum::routing::get(agent::health))
        // WebSocket PTY
        .route(
            "/agent/:machine_id/pty",
            axum::routing::get(pty::handle_pty_ws),
        )
        .route(
            "/agent/:machine_id/audit",
            axum::routing::get(pty::handle_audit_ws),
        )
        // Structured exec (JSON command → JSON result)
        .route(
            "/agent/:machine_id/exec",
            axum::routing::post(exec::exec_command),
        )
        // Task lifecycle
        .route("/agent/task", axum::routing::post(tasks::create_task))
        .route("/agent/task/:id", axum::routing::get(tasks::get_task))
        .route(
            "/agent/task/:id/result",
            axum::routing::post(tasks::submit_result),
        )
        // Task SSE stream
        .route(
            "/agent/task/:id/stream",
            axum::routing::get(tasks::stream_task),
        )
        // Mid-session reprompt
        .route(
            "/agent/:machine_id/instruct",
            axum::routing::post(instruct::inject),
        )
        // Credential vault (admin CRUD)
        .route("/admin/credentials", axum::routing::post(credentials::create_credential))
        .route("/admin/credentials", axum::routing::get(credentials::list_credentials))
        .route("/admin/credentials/:id", axum::routing::get(credentials::get_credential))
        .route("/admin/credentials/:id", axum::routing::delete(credentials::delete_credential))
        .route("/admin/credentials/:id/rotate", axum::routing::post(credentials::rotate_credential))
        // Credential vault (agent access)
        .route(
            "/agent/:machine_id/credentials/:name",
            axum::routing::get(credentials::agent_get_credential),
        )
        // Git flow
        .route("/agent/:machine_id/git/clone", axum::routing::post(git::clone_repo))
        .route("/agent/:machine_id/git/branch", axum::routing::post(git::create_branch))
        .route("/agent/:machine_id/git/commit", axum::routing::post(git::commit))
        .route("/agent/:machine_id/git/push", axum::routing::post(git::push))
        .route("/agent/:machine_id/git/pr", axum::routing::post(git::create_pr))
        .route("/agent/:machine_id/git/status", axum::routing::get(git::status))
        .route("/agent/:machine_id/git/log", axum::routing::get(git::log))
        // Workflows
        .route("/workflow", axum::routing::post(workflows::create_workflow))
        .route("/workflow/:id", axum::routing::get(workflows::get_workflow))
        .route("/workflow", axum::routing::get(workflows::list_workflows))
        .route("/workflow/:id/run", axum::routing::post(workflows::run_workflow))
        .route("/workflow/run/:id", axum::routing::get(workflows::get_run))
        // Agent-to-agent message bus
        .route("/agent/:machine_id/messages", axum::routing::post(messages::post_message))
        .route("/agent/:machine_id/messages", axum::routing::get(messages::poll_messages))
        .route("/agent/:machine_id/messages/stream", axum::routing::get(messages::stream_messages))
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
        // Prometheus metrics
        .route("/metrics", axum::routing::get(metrics::get_metrics))
        // Static files (phone enrollment PWA)
        .nest_service("/static", tower_http::services::ServeDir::new("../phone"))
        // ── Middleware layers (applied bottom-up; last call = outermost) ──
        //
        // 1. Concurrency limit — caps in-flight requests to prevent overload.
        //    Applied first (inner) so that TraceLayer still logs the 503.
        .layer(middleware::from_fn_with_state(
            concurrency_semaphore,
            concurrency_limit,
        ))
        // 2. Request tracing — logs method, URI, status code, and latency for
        //    every HTTP request. Applied last (outermost) so that *all*
        //    responses — including 503s from the concurrency limiter — are
        //    logged.
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());
    (router, state)
}

/// Build the main axum router with all routes (legacy — returns Router only).
pub fn build_router_simple(
    db_pool: Pool<SqliteConnectionManager>,
    audit_keys: AuditKeys,
    push_keys: PushKeys,
) -> Router {
    build_router(db_pool, audit_keys, push_keys).0
}
