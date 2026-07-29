//! Prometheus `/metrics` endpoint.
//!
//! Exposes gateway operational metrics in Prometheus exposition format
//! so operators can scrape them with a standard Prometheus instance.
//!
//! Currently exposed metrics:
//! - `stronghold_sessions_active` — gauge: number of active sessions.
//! - `stronghold_approvals_pending` — gauge: number of pending approval requests.
//! - `stronghold_audit_entries_total` — counter: total audit log entries.

use crate::routes::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

/// GET `/metrics` — return Prometheus-format metrics text.
pub async fn get_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.get().unwrap();
    let active_sessions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM machines WHERE status = 'active'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let pending_approvals: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_sessions WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let audit_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM audit_entries", [], |r| r.get(0))
        .unwrap_or(0);

    let body = format!(
        "# HELP stronghold_sessions_active Number of active sessions\n\
         # TYPE stronghold_sessions_active gauge\n\
         stronghold_sessions_active {}\n\
         # HELP stronghold_approvals_pending Number of pending approval requests\n\
         # TYPE stronghold_approvals_pending gauge\n\
         stronghold_approvals_pending {}\n\
         # HELP stronghold_audit_entries_total Total audit log entries\n\
         # TYPE stronghold_audit_entries_total counter\n\
         stronghold_audit_entries_total {}\n",
        active_sessions, pending_approvals, audit_entries
    );
    (StatusCode::OK, body)
}
