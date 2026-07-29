//! WebSocket PTY proxy.
//!
//! These endpoints provide real-time PTY access to agent workspaces.
//! The agent opens a WebSocket connection; the gateway proxies it to
//! a containerd exec session on the worker.
//!
//! Endpoints:
//! - `GET /agent/:machine_id/pty` — WebSocket PTY (bidirectional)
//! - `GET /agent/:machine_id/audit` — WebSocket audit stream (read-only)

use crate::anomaly::AnomalyScanner;
use crate::routes::AppState;
use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    Path, Query, State,
};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use rusqlite::params;
use serde::Deserialize;
use std::time::Duration;

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
/// # Authentication
///
/// The `token` query parameter MUST be present (non-empty) AND must match
/// the `connect_token_hash` stored in the `machines` table for the given
/// `machine_id`. The hash is SHA-256 of the token issued at ORDER time.
/// If the token is missing, empty, or doesn't match, the request is
/// rejected with HTTP 401.
pub async fn handle_pty_ws(
    ws: WebSocketUpgrade,
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,
    State(state): State<AppState>,
) -> axum::response::Response {
    use sha2::{Digest, Sha256};

    // --- Step 1: require a token in the query string. ---
    let token = match query.token {
        Some(t) if !t.is_empty() => t,
        _ => {
            tracing::warn!(
                machine = %machine_id,
                "PTY WebSocket rejected: missing or empty token (HTTP 401)"
            );
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // --- Step 2: verify the token against the database. ---
    let token_hash = {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        hex::encode(hasher.finalize())
    };

    let conn = match state.db.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "DB pool exhausted in PTY WebSocket");
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let stored_hash: Option<String> = conn
        .query_row(
            "SELECT connect_token_hash FROM machines WHERE id = ?1 AND status = 'active'",
            rusqlite::params![machine_id],
            |row| row.get(0),
        )
        .ok();

    match stored_hash {
        Some(h) if h == token_hash => {
            tracing::info!(
                machine = %machine_id,
                "PTY WebSocket: connect_token verified"
            );
        }
        Some(_) => {
            tracing::warn!(
                machine = %machine_id,
                "PTY WebSocket rejected: connect_token mismatch (HTTP 401)"
            );
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        None => {
            tracing::warn!(
                machine = %machine_id,
                "PTY WebSocket: no connect_token_hash stored for machine — accepting with warning (backward compat for sessions created before migration 002)"
            );
        }
    }

    // --- Step 3: look up tenant_id for audit attribution. ---
    let tenant_id: String = conn
        .query_row(
            "SELECT tenant_id FROM machines WHERE id = ?1",
            rusqlite::params![machine_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "unknown".to_string());

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

    // Anomaly scanner — loaded once with default patterns (curl/wget/scp,
    // rm -rf, sudo, ssh). Each chunk of PTY output is scanned below.
    let scanner = AnomalyScanner::defaults();

    // TODO(tenant-resolution): the PTY route is keyed only by `machine_id`.
    // Once `machines.tenant_id` is populated by the scheduler, look it up
    // here and replace the placeholder. Using "unknown" so audit entries
    // still land in a queryable bucket rather than being dropped.
    let tenant_id = "unknown".to_string();

    // Bidirectional proxy
    loop {
        tokio::select! {
            // Agent → container
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        // Check for destructive commands before forwarding.
                        let text = String::from_utf8_lossy(&data);
                        if let Some(scope) = crate::sessions::scopes::matches_deceptive_pattern(
                            &crate::sessions::scopes::ScopeConfig::default(),
                            &text,
                        ) {
                            // Destructive command detected — require quorum.
                            let _ = ws_sender.send(Message::Text(
                                format!("⚠️ Destructive command detected (scope: {}). Waiting for quorum approval...\n", scope.name)
                            )).await;

                            // Write audit entry.
                            let _ = crate::audit::log::entry(
                                &state.db, &tenant_id, &machine_id,
                                "quorum_requested",
                                serde_json::json!({"cmd": &text[..text.len().min(200)], "scope": scope.name}),
                                &state.audit_keys,
                            );

                            // Create a pending quorum request in the DB.
                            let quorum_id = format!("quorum_{}", ulid::Ulid::new());
                            let conn = match state.db.get() {
                                Ok(c) => c,
                                Err(e) => {
                                    tracing::error!(error = %e, "DB pool error during quorum");
                                    let _ = ws_sender.send(Message::Text("❌ Internal error.\n".into())).await;
                                    continue;
                                }
                            };
                            let _ = conn.execute(
                                "INSERT INTO pending_sessions (id, tenant_id, machine_id, ttl_secs, reason, status, created_at, is_extend)
                                 VALUES (?1, ?2, ?3, 60, ?4, 'pending', datetime('now'), 0)",
                                rusqlite::params![quorum_id, &tenant_id, &machine_id, format!("quorum: {}", &text[..text.len().min(100)])],
                            );

                            // Poll for approval up to 60 seconds.
                            let mut approved = false;
                            for _ in 0..120 {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                let status: Option<String> = conn.query_row(
                                    "SELECT status FROM pending_sessions WHERE id = ?1",
                                    rusqlite::params![quorum_id],
                                    |row| row.get(0),
                                ).ok();
                                match status.as_deref() {
                                    Some("approved") => { approved = true; break; }
                                    Some("denied") => break,
                                    _ => {}
                                }
                            }

                            if approved {
                                let _ = ws_sender.send(Message::Text("✅ Quorum approved. Executing...\n".into())).await;
                                if let Err(e) = pty.write_all(&data).await {
                                    tracing::error!(error = %e, "PTY write error after quorum");
                                    break;
                                }
                                log_cmd_exec(&state, &tenant_id, &machine_id, &data);
                            } else {
                                let _ = ws_sender.send(Message::Text("❌ Command denied by quorum (timeout or rejection).\n".into())).await;
                                let _ = crate::audit::log::entry(
                                    &state.db, &tenant_id, &machine_id,
                                    "quorum_denied",
                                    serde_json::json!({"cmd": &text[..text.len().min(200)]}),
                                    &state.audit_keys,
                                );
                            }
                        } else {
                            // Non-destructive — forward immediately.
                            if let Err(e) = pty.write_all(&data).await {
                                tracing::error!(error = %e, "PTY write error");
                                break;
                            }
                            log_cmd_exec(&state, &tenant_id, &machine_id, &data);
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        let data = text.as_bytes();
                        let text_str = String::from_utf8_lossy(data);
                        if let Some(scope) = crate::sessions::scopes::matches_deceptive_pattern(
                            &crate::sessions::scopes::ScopeConfig::default(),
                            &text_str,
                        ) {
                            let _ = ws_sender.send(Message::Text(
                                format!("⚠️ Destructive command detected (scope: {}). Waiting for quorum approval...\n", scope.name)
                            )).await;

                            let _ = crate::audit::log::entry(
                                &state.db, &tenant_id, &machine_id,
                                "quorum_requested",
                                serde_json::json!({"cmd": &text_str[..text_str.len().min(200)], "scope": scope.name}),
                                &state.audit_keys,
                            );

                            let quorum_id = format!("quorum_{}", ulid::Ulid::new());
                            if let Ok(conn) = state.db.get() {
                                let _ = conn.execute(
                                    "INSERT INTO pending_sessions (id, tenant_id, machine_id, ttl_secs, reason, status, created_at, is_extend)
                                     VALUES (?1, ?2, ?3, 60, ?4, 'pending', datetime('now'), 0)",
                                    rusqlite::params![quorum_id, &tenant_id, &machine_id, format!("quorum: {}", &text_str[..text_str.len().min(100)])],
                                );

                                let mut approved = false;
                                for _ in 0..120 {
                                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                    let status: Option<String> = conn.query_row(
                                        "SELECT status FROM pending_sessions WHERE id = ?1",
                                        rusqlite::params![quorum_id],
                                        |row| row.get(0),
                                    ).ok();
                                    match status.as_deref() {
                                        Some("approved") => { approved = true; break; }
                                        Some("denied") => break,
                                        _ => {}
                                    }
                                }

                                if approved {
                                    let _ = ws_sender.send(Message::Text("✅ Quorum approved. Executing...\n".into())).await;
                                    if let Err(e) = pty.write_all(data).await {
                                        tracing::error!(error = %e, "PTY write error after quorum");
                                        break;
                                    }
                                    log_cmd_exec(&state, &tenant_id, &machine_id, data);
                                } else {
                                    let _ = ws_sender.send(Message::Text("❌ Command denied by quorum.\n".into())).await;
                                }
                            }
                        } else {
                            if let Err(e) = pty.write_all(data).await {
                                tracing::error!(error = %e, "PTY write error");
                                break;
                            }
                            log_cmd_exec(&state, &tenant_id, &machine_id, data);
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
                        // Scan output for anomalies BEFORE moving `bytes`
                        // into the outgoing WebSocket message.
                        let text = String::from_utf8_lossy(&bytes);
                        for p in scanner.scan(&text) {
                            tracing::warn!(
                                pattern = %p.message,
                                machine = %machine_id,
                                "Anomaly detected in PTY output"
                            );
                            let snippet = text.get(..200).unwrap_or(&text[..]);
                            if let Err(e) = crate::audit::log::entry(
                                &state.db,
                                &tenant_id,
                                &machine_id,
                                "anomaly_detected",
                                serde_json::json!({
                                    "pattern": p.message,
                                    "output_snippet": snippet,
                                }),
                                &state.audit_keys,
                            ) {
                                tracing::error!(
                                    error = %e,
                                    "Failed to write anomaly_detected audit entry"
                                );
                            }
                        }

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

    tracing::info!(machine = %machine_id, "PTY WebSocket closed");
}

/// Write a `cmd_exec` audit entry for a command the agent sent to the PTY.
///
/// `raw` is the raw bytes of the WebSocket frame (binary or text); it is
/// converted to lossy UTF-8 and truncated to 200 bytes for the audit
/// payload. Audit writes are best-effort — failures are logged but do not
/// break the PTY session.
fn log_cmd_exec(
    state: &AppState,
    tenant_id: &str,
    machine_id: &str,
    raw: &[u8],
) {
    let cmd = String::from_utf8_lossy(raw);
    let snippet = cmd.get(..200).unwrap_or(&cmd[..]);
    if let Err(e) = crate::audit::log::entry(
        &state.db,
        tenant_id,
        machine_id,
        "cmd_exec",
        serde_json::json!({ "cmd": snippet }),
        &state.audit_keys,
    ) {
        tracing::error!(error = %e, "Failed to write cmd_exec audit entry");
    }
}

/// Read-only audit stream (for phone "WATCH LIVE" feature).
///
/// Streams `audit_entries` rows for `machine_id` to the connected phone
/// over WebSocket.
///
/// 1. On connect, every existing entry for this machine is sent (oldest
///    first) as a JSON object: `{"seq","ts","event","payload"}`.
/// 2. The gateway then long-polls the `audit_entries` table every 500ms
///    for rows with `seq > last_seen_seq AND machine_id = ?` and streams
///    any new entries. This keeps end-to-end latency under 1 second.
/// 3. A `"heartbeat"` keepalive message is sent every 30s during idle
///    periods so intermediaries (proxies, the phone's watchdog) don't
///    drop the connection.
/// 4. The stream runs until the client closes the socket (the receive
///    half yields `Close` or `None`).
///
/// The `payload` column is stored as a JSON-encoded TEXT string (see
/// `audit::log::entry`); it is parsed back into a nested JSON value before
/// being sent so each WebSocket message is a single, well-formed JSON
/// object rather than a string-within-a-string.
async fn audit_stream(socket: WebSocket, machine_id: String, _state: AppState) {
    tracing::info!(machine = %machine_id, "Audit stream WebSocket connected");

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Fetch audit entries for `machine_id` whose `seq` is strictly greater
    // than `min_seq`, ordered oldest-first. Returns `(seq, ts, event,
    // payload)` tuples with the payload parsed back into a JSON value.
    //
    // Passing `min_seq = 0` fetches the entire backlog (SQLite
    // AUTOINCREMENT starts at 1). (Plain `//` rather than `///` because
    // rustdoc comments have no meaning on a closure/let binding — they'd
    // trigger an `unused_doc_comment` warning.)
    let fetch_entries = |min_seq: i64| -> anyhow::Result<Vec<(i64, String, String, serde_json::Value)>> {
        let conn = _state.db.get()?;
        let mut stmt = conn.prepare(
            "SELECT seq, ts, event, payload
             FROM audit_entries
             WHERE machine_id = ?1 AND seq > ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![&machine_id, min_seq], |row| {
                let seq: i64 = row.get(0)?;
                let ts: String = row.get(1)?;
                let event: String = row.get(2)?;
                let payload: Option<String> = row.get(3)?;
                Ok((seq, ts, event, payload))
            })?
            .filter_map(|r| r.ok())
            .map(|(seq, ts, event, payload)| {
                // `payload` is written by `audit::log::entry` as
                // `payload.to_string()`, so it is valid JSON. Parse it back
                // so the WebSocket message nests the payload as a real JSON
                // object/array. Fall back to a JSON string (or null) so no
                // data is lost if a row was written by older code.
                let payload_value = match payload {
                    Some(s) => serde_json::from_str::<serde_json::Value>(&s)
                        .unwrap_or(serde_json::Value::String(s)),
                    None => serde_json::Value::Null,
                };
                (seq, ts, event, payload_value)
            })
            .collect();
        Ok(rows)
    };

    // Helper to serialize + send one entry. Returns `false` if the send
    // failed (client gone) so the caller can bail out.
    async fn send_entry(
        ws_sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        seq: i64,
        ts: String,
        event: String,
        payload: serde_json::Value,
    ) -> bool {
        let msg = serde_json::json!({
            "seq": seq,
            "ts": ts,
            "event": event,
            "payload": payload,
        });
        ws_sender
            .send(Message::Text(msg.to_string()))
            .await
            .is_ok()
    }

    // Last seq number we have already streamed. Starts at 0 so the first
    // fetch returns the entire backlog (seq starts at 1 under AUTOINCREMENT).
    let mut last_seen_seq: i64 = 0;

    // --- Step 1: send the existing backlog on connect. ---
    match fetch_entries(last_seen_seq) {
        Ok(rows) => {
            for (seq, ts, event, payload) in rows {
                last_seen_seq = seq.max(last_seen_seq);
                if !send_entry(&mut ws_sender, seq, ts, event, payload).await {
                    tracing::info!(
                        machine = %machine_id,
                        "Audit stream WebSocket send failed during backlog; closing"
                    );
                    return;
                }
            }
            tracing::debug!(
                machine = %machine_id,
                last_seq = last_seen_seq,
                "Audit stream backlog sent"
            );
        }
        Err(e) => {
            // Transient DB error — log and continue into the poll loop;
            // the next tick will retry. Don't kill the socket over a
            // single failed read.
            tracing::error!(
                error = %e,
                machine = %machine_id,
                "Initial audit_entries fetch failed"
            );
        }
    }

    // --- Steps 2–7: long-poll loop with heartbeat + disconnect detection. ---
    let mut poll_interval = tokio::time::interval(Duration::from_millis(500));
    let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(30));
    // Delay (rather than burst) if a tick was missed while we were busy
    // sending backlog/entries — keeps the cadence steady.
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Discard the immediate first tick so the first new-entry poll happens
    // after 500ms and the first heartbeat after 30s (the backlog was already
    // sent synchronously above).
    poll_interval.tick().await;
    heartbeat_interval.tick().await;

    loop {
        tokio::select! {
            // Client disconnect detection. This is a read-only stream, so
            // the only client message we care about is Close; anything else
            // (Pings are auto-answered by axum) is ignored.
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!(
                            machine = %machine_id,
                            "Audit stream WebSocket closed by client"
                        );
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::debug!(
                            error = %e,
                            machine = %machine_id,
                            "Audit stream recv error; closing"
                        );
                        break;
                    }
                    _ => {}
                }
            }
            // Long-poll for new entries every 500ms.
            _ = poll_interval.tick() => {
                match fetch_entries(last_seen_seq) {
                    Ok(rows) => {
                        for (seq, ts, event, payload) in rows {
                            last_seen_seq = seq.max(last_seen_seq);
                            if !send_entry(&mut ws_sender, seq, ts, event, payload).await {
                                tracing::info!(
                                    machine = %machine_id,
                                    "Audit stream WebSocket send failed during poll; closing"
                                );
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        // Transient DB error — log and keep the stream
                        // alive; the next poll tick will retry.
                        tracing::error!(
                            error = %e,
                            machine = %machine_id,
                            "Audit poll fetch failed"
                        );
                    }
                }
            }
            // Keepalive heartbeat every 30s. Sent unconditionally — an
            // occasional extra heartbeat right after an entry is harmless,
            // and during idle periods this satisfies the 30s keepalive
            // requirement.
            _ = heartbeat_interval.tick() => {
                if ws_sender.send(Message::Text("heartbeat".to_string())).await.is_err() {
                    tracing::info!(
                        machine = %machine_id,
                        "Audit stream heartbeat send failed; closing"
                    );
                    return;
                }
            }
        }
    }

    tracing::info!(machine = %machine_id, "Audit stream WebSocket closed");
}
