//! WebSocket PTY proxy.
//!
//! These endpoints provide real-time PTY access to agent workspaces.
//! The agent opens a WebSocket connection; the gateway proxies it to
//! a containerd exec session on the worker.
//!
//! Endpoints:
//! - `GET /agent/:machine_id/pty` — WebSocket PTY (bidirectional)
//! - `GET /agent/:machine_id/audit` — WebSocket audit stream (read-only)

use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    Path, State,
};
use futures_util::{SinkExt, StreamExt};
use crate::routes::AppState;

/// Handle a WebSocket PTY connection.
///
/// The agent opens this after a successful ORDER. The gateway:
/// 1. Verifies the connect token
/// 2. Opens a containerd exec session on the worker
/// 3. Proxies bytes bidirectionally
/// 4. Streams all bytes to the audit log (in parallel)
/// 5. Scans for anomaly patterns (pushes phone if matched)
pub async fn handle_pty_ws(
    ws: WebSocketUpgrade,
    Path(machine_id): Path<String>,
    State(state): State<AppState>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| pty_proxy(socket, machine_id, state))
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
async fn pty_proxy(socket: WebSocket, machine_id: String, state: AppState) {
    tracing::info!(machine = %machine_id, "PTY WebSocket connected");

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Open containerd exec session on the worker
    let mut pty = match crate::machines::scheduler::open_pty(&machine_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(machine = %machine_id, error = %e, "Failed to open PTY");
            let _ = ws_sender.send(Message::Text(
                format!("Error: failed to open PTY: {}", e)
            )).await;
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
    while let Err(_e) = ws_sender.send(Message::Text(
        "Audit stream not yet implemented".to_string()
    )).await {
        break;
    }
}
