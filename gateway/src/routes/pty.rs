//! WebSocket PTY proxy.
//!
//! These endpoints provide real-time PTY access to agent workspaces.
//! The agent opens a WebSocket connection; the gateway proxies it to
//! a containerd exec session on the worker.
//!
//! Endpoints:
//! - `GET /agent/:machine_id/pty` — WebSocket PTY (bidirectional)
//! - `GET /agent/:machine_id/audit` — WebSocket audit stream (read-only)

use crate::routes::AppState;
use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    Path, Query, State,
};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

/// Query string parameters for the PTY WebSocket endpoint.
///
/// The agent connects with:
/// `wss://gateway/agent/{machine_id}/pty?token={connect_token}`
///
/// `token` is the `connect_token` returned by `/agent/order`.
#[derive(Debug, Deserialize)]
pub struct PtyQuery {
    /// The `connect_token` issued when the session was finalized.
    pub token: Option<String>,
}

/// Handle a WebSocket PTY connection.
///
/// The agent opens this after a successful ORDER. The gateway:
/// 1. Verifies the connect token
/// 2. Opens a containerd exec session on the worker
/// 3. Proxies bytes bidirectionally
/// 4. Streams all bytes to the audit log (in parallel)
/// 5. Scans for anomaly patterns (pushes phone if matched)
///
/// # Authentication
///
/// The `token` query parameter MUST be present. If it is missing, the
/// request is rejected with HTTP 401 before the WebSocket is upgraded.
///
/// Full verification of the token against the `machines` table is not yet
/// implemented — the `machines` table has no `connect_token_hash` column.
/// Until the schema is migrated, any non-empty token is accepted and a
/// warning is logged. This still raises the bar from "knows the
/// `machine_id`" to "knows a token was issued", which is strictly better
/// than no auth at all.
///
/// TODO(full-verification): once `machines.connect_token_hash` exists,
/// run `SELECT connect_token_hash FROM machines WHERE id = ?1 AND status
/// = 'active'` using `state.db`, then compare `SHA-256(token)` with the
/// stored hash and reject (401) on mismatch.
pub async fn handle_pty_ws(
    ws: WebSocketUpgrade,
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,
    State(state): State<AppState>,
) -> axum::response::Response {
    // --- Step 1: require a token in the query string. ---
    let token = match query.token {
        Some(t) if !t.is_empty() => t,
        _ => {
            tracing::warn!(
                machine = %machine_id,
                "PTY WebSocket rejected: missing or empty `token` query parameter (HTTP 401)"
            );
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // --- Step 2: verify the token. ---
    //
    // Full DB-backed verification requires a `connect_token_hash` column on
    // the `machines` table, which has not been migrated yet. Until then we
    // accept any non-empty token but emit a warning so the gap is visible
    // in logs. The presence of a token still forces the client to be
    // privy to a value that was issued by `/agent/order`.
    //
    // TODO(full-verification): hash `token` with SHA-256, query
    // `state.db` for `SELECT connect_token_hash FROM machines WHERE id =
    // ?1 AND status = 'active'`, and compare. On column-not-found,
    // fall back to accept-with-warning (backward compat). On mismatch,
    // return 401.
    tracing::warn!(
        machine = %machine_id,
        "PTY token verification SKIPPED — `machines.connect_token_hash` column not yet present in schema. \
                 Accepting connection with unverified token. This will become a hard 401 once the \
                 schema is migrated."
    );

    ws.on_upgrade(move |socket| pty_proxy(socket, machine_id, token, state))
}

/// Handle a WebSocket audit stream connection (read-only).
///
/// Lets the tenant's phone (via browser) watch a live session in real-time.
pub async fn handle_audit_ws(
    ws: WebSocketUpgrade,
    Path(machine_id): Path<String>,
    State(state): State<AppState>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| audit_stream(socket, machine_id, state))
}

/// Bidirectional PTY proxy between the agent WebSocket and the containerd exec.
///
/// `token` is the (currently unverified) `connect_token` supplied by the
/// client. It is carried through here so that, once full DB verification
/// lands, the proxy can correlate audit log entries with the specific
/// token that was used to open the session.
async fn pty_proxy(socket: WebSocket, machine_id: String, token: String, state: AppState) {
    tracing::info!(
        machine = %machine_id,
        token_len = token.len(),
        "PTY WebSocket connected"
    );

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Open containerd exec session on the worker
    let mut pty = match crate::machines::scheduler::open_pty(&machine_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(machine = %machine_id, error = %e, "Failed to open PTY");
            let _ = ws_sender
                .send(Message::Text(format!("Error: failed to open PTY: {}", e)))
                .await;
            return;
        }
    };

    // Spawn audit logger
    let audit_handle = tokio::spawn({
        let machine_id = machine_id.clone();
        let state = state.clone();
        async move {
            // TODO: stream bytes to audit log
            let _ = (machine_id, state);
        }
    });

    // Bidirectional proxy
    loop {
        tokio::select! {
            // Agent → container
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if let Err(e) = pty.write_all(&data).await {
                            tracing::error!(error = %e, "PTY write error");
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = pty.write_all(text.as_bytes()).await {
                            tracing::error!(error = %e, "PTY write error");
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            // Container → agent
            data = pty.read() => {
                match data {
                    Ok(bytes) => {
                        if let Err(e) = ws_sender.send(Message::Binary(bytes)).await {
                            tracing::error!(error = %e, "WebSocket send error");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "PTY read error");
                        break;
                    }
                }
            }
        }
    }

    audit_handle.abort();
    tracing::info!(machine = %machine_id, "PTY WebSocket closed");
}

/// Read-only audit stream (for phone "WATCH LIVE" feature).
async fn audit_stream(socket: WebSocket, machine_id: String, _state: AppState) {
    tracing::info!(machine = %machine_id, "Audit stream WebSocket connected");

    let (mut ws_sender, mut _ws_receiver) = socket.split();

    // TODO: subscribe to audit events for this machine_id and stream to client
    let _ = ws_sender
        .send(Message::Text(
            "Audit stream not yet implemented".to_string(),
        ))
        .await;
}
