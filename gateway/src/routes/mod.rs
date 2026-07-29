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
pub mod metrics;
pub mod phone;
pub mod pty;

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
) -> Router {
    let state = AppState {
        db: db_pool,
        audit_keys,
        push_keys,
    };

    // Global concurrency limiter — shared across all routes.
    let concurrency_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));

    Router::new()
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
        .with_state(state)
}
