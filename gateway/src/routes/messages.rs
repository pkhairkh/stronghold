//! Agent-to-agent message bus endpoints.
//!
//! Agents communicate with each other through a simple message-bus model
//! backed by the `agent_messages` SQLite table (migration 003). A message
//! has a `from_machine`, an optional `to_machine` (`NULL` = broadcast),
//! a `channel` string, and an arbitrary JSON `body`. Recipients either
//! poll the bus with `GET /agent/:machine_id/messages` or subscribe to a
//! live `SSE` stream at `GET /agent/:machine_id/messages/stream`.
//!
//! # Endpoints
//!
//! | Method | Path                                  | Handler            |
//! |--------|---------------------------------------|--------------------|
//! | POST   | `/agent/:machine_id/messages`         | [`post_message`]   |
//! | GET    | `/agent/:machine_id/messages`         | [`poll_messages`]  |
//! | GET    | `/agent/:machine_id/messages/stream`  | [`stream_messages`]|
//!
//! All three endpoints require a valid `connect_token` issued at ORDER time
//! (same SHA-256 → `machines.connect_token_hash` comparison as `pty.rs` /
//! `exec.rs` / `git.rs`). The token is supplied via the `?token=` query
//! parameter (the [`PtyQuery`] pattern).

use crate::routes::pty::PtyQuery;
use crate::routes::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::Json;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

// ============================================================================
// Request / response types
// ============================================================================

/// Request body for `POST /agent/:machine_id/messages`.
#[derive(Debug, Deserialize)]
pub struct PostMessageRequest {
    /// Recipient machine ID. `None` (or `null`) means **broadcast** — any
    /// machine polling the channel will receive the message.
    pub to: Option<String>,
    /// Logical channel name (e.g. `"build-events"`, `"tasks.coord"`).
    /// Both sender and receiver must agree on the channel string.
    pub channel: String,
    /// Arbitrary JSON payload. Stored verbatim (serialized to a JSON string)
    /// in `agent_messages.body` and re-parsed on read.
    pub body: serde_json::Value,
}

/// Response body for `POST /agent/:machine_id/messages`.
#[derive(Debug, Serialize)]
pub struct PostMessageResponse {
    /// The auto-incremented rowid of the new `agent_messages` row.
    pub id: i64,
    /// `created_at` timestamp as stored by `datetime('now')` (UTC,
    /// `YYYY-MM-DD HH:MM:SS`).
    pub created_at: String,
}

/// Query string for `GET /agent/:machine_id/messages`.
///
/// Combines the `connect_token` (same `PtyQuery` pattern as the rest of the
/// agent API) with the `channel` and `since` filters.
#[derive(Debug, Deserialize)]
pub struct MessagesPollQuery {
    /// `connect_token` issued at ORDER time.
    pub token: Option<String>,
    /// Channel to filter on.
    pub channel: String,
    /// Lower bound (exclusive) for `created_at`. Only rows whose
    /// `created_at` is strictly greater than this value are returned.
    pub since: String,
}

/// Query string for `GET /agent/:machine_id/messages/stream`.
#[derive(Debug, Deserialize)]
pub struct MessagesStreamQuery {
    /// `connect_token` issued at ORDER time.
    pub token: Option<String>,
    /// Channel to stream.
    pub channel: String,
}

/// A single message in the `GET /agent/:machine_id/messages` response.
///
/// Also the JSON payload emitted on each `message` SSE event by
/// [`stream_messages`].
#[derive(Debug, Serialize)]
pub struct Message {
    pub id: i64,
    pub from_machine: String,
    /// `None` for broadcast messages (recipient-agnostic).
    pub to_machine: Option<String>,
    pub channel: String,
    /// Parsed JSON payload. Falls back to `Value::Null` if the stored
    /// `body` text is not valid JSON (which would indicate DB corruption).
    pub body: serde_json::Value,
    pub created_at: String,
}

// ============================================================================
// Handlers
// ============================================================================

/// Post a message to the agent bus.
///
/// Verifies the `connect_token`, then inserts a row into `agent_messages`
/// with `from_machine = machine_id`, `to_machine = req.to`, `channel`,
/// and `body` serialized to a JSON string. Returns the new row's `id`
/// and `created_at`.
pub async fn post_message(
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,
    State(state): State<AppState>,
    Json(req): Json<PostMessageRequest>,
) -> Result<Json<PostMessageResponse>, (StatusCode, String)> {
    let _tenant_id = verify_connect_token(&state, &machine_id, &query).await?;

    let body_str = req.body.to_string();

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    conn.execute(
        "INSERT INTO agent_messages (from_machine, to_machine, channel, body, created_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))",
        rusqlite::params![&machine_id, req.to, &req.channel, &body_str],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let id = conn.last_insert_rowid();

    // Re-read the row to get the authoritative `created_at` stamped by
    // `datetime('now')` (avoiding clock-skew between client and server).
    let created_at: String = conn
        .query_row(
            "SELECT created_at FROM agent_messages WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(
        machine = %machine_id,
        message_id = id,
        channel = %req.channel,
        to = ?req.to,
        "Agent message posted"
    );

    Ok(Json(PostMessageResponse { id, created_at }))
}

/// Poll for messages addressed to `machine_id` (or broadcast).
///
/// Returns all rows where `(to_machine = machine_id OR to_machine IS NULL)
/// AND channel = ? AND created_at > ?`, ordered oldest-first by `id`.
pub async fn poll_messages(
    Path(machine_id): Path<String>,
    Query(query): Query<MessagesPollQuery>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Message>>, (StatusCode, String)> {
    // Reuse the standard connect_token check by adapting the combined
    // query into a `PtyQuery` view.
    let pty_query = PtyQuery {
        token: query.token.clone(),
    };
    let _tenant_id = verify_connect_token(&state, &machine_id, &pty_query).await?;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, from_machine, to_machine, channel, body, created_at
             FROM agent_messages
             WHERE (to_machine = ?1 OR to_machine IS NULL)
               AND channel = ?2
               AND created_at > ?3
             ORDER BY id ASC",
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows = stmt
        .query_map(
            rusqlite::params![&machine_id, &query.channel, &query.since],
            |row| {
                let id: i64 = row.get(0)?;
                let from_machine: String = row.get(1)?;
                let to_machine: Option<String> = row.get(2)?;
                let channel: String = row.get(3)?;
                let body_str: String = row.get(4)?;
                let created_at: String = row.get(5)?;
                // `body` is written by this module as a JSON string, so a
                // parse failure indicates DB corruption — surface it as
                // null rather than panicking the poll.
                let body: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                Ok(Message {
                    id,
                    from_machine,
                    to_machine,
                    channel,
                    body,
                    created_at,
                })
            },
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut messages = Vec::new();
    for row in rows {
        let msg = row.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        messages.push(msg);
    }

    Ok(Json(messages))
}

/// Stream new agent messages as Server-Sent Events.
///
/// Polls the database every 500 ms for messages newer than the last one
/// yielded on this stream (matching `(to_machine = machine_id OR to_machine
/// IS NULL) AND channel = ?`). Each new message is emitted as a `message`
/// SSE event whose data is a JSON-encoded [`Message`]. A `heartbeat` data
/// event is sent every 30 s during idle periods to keep the connection
/// alive through proxies.
///
/// The stream runs indefinitely — the client closes it by disconnecting.
/// The `connect_token` is verified once at connection time; subsequent
/// polls trust the already-authenticated stream (matching the
/// `pending_approval_stream` / `stream_task` pattern).
pub async fn stream_messages(
    Path(machine_id): Path<String>,
    Query(query): Query<MessagesStreamQuery>,
    State(state): State<AppState>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, (StatusCode, String)>
{
    // Verify the connect_token once at connection time; subsequent polls
    // trust the already-authenticated stream.
    let pty_query = PtyQuery {
        token: query.token.clone(),
    };
    let _tenant_id = verify_connect_token(&state, &machine_id, &pty_query).await?;

    // Clone the owned values the stream needs so it is `'static`.
    let db = state.db.clone();
    let channel = query.channel.clone();
    let machine_id_owned = machine_id.clone();

    let stream = async_stream::stream! {
        // Last message `id` we have already yielded. Starts at 0 so the
        // first poll returns the entire backlog (AUTOINCREMENT starts at 1).
        let mut last_id: i64 = 0;

        let mut poll_interval = tokio::time::interval(Duration::from_millis(500));
        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(30));
        // Delay (rather than burst) if a tick is missed while we were busy
        // yielding events — keeps the cadence steady.
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Discard the immediate first tick so the first poll happens after
        // 500ms and the first heartbeat after 30s.
        poll_interval.tick().await;
        heartbeat_interval.tick().await;

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    match fetch_new_messages(&db, &machine_id_owned, &channel, last_id) {
                        Ok(rows) => {
                            for msg in rows {
                                last_id = msg.id.max(last_id);
                                // Serialize the Message struct to a JSON
                                // value for the SSE `data:` field. Falls
                                // back to null on serialization failure
                                // (which should never happen for a
                                // Serialize-derived struct).
                                let payload = serde_json::to_value(&msg)
                                    .unwrap_or(serde_json::Value::Null);
                                yield Ok(Event::default()
                                    .event("message")
                                    .data(payload.to_string()));
                            }
                        }
                        Err(e) => {
                            // Transient DB error — log and keep the stream
                            // alive; the next poll tick will retry.
                            tracing::error!(
                                error = %e,
                                machine = %machine_id_owned,
                                channel = %channel,
                                "Failed to poll agent_messages for SSE stream"
                            );
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    yield Ok(Event::default().data("heartbeat"));
                }
            }
        }
    };

    Ok(Sse::new(stream))
}

// ============================================================================
// Helpers
// ============================================================================

/// Verify the `connect_token` against `machines.connect_token_hash`.
///
/// Same SHA-256 → stored-hash comparison as `pty.rs` / `exec.rs` / `git.rs`.
/// Returns the `tenant_id` for audit attribution on success.
///
/// # Errors
///
/// Returns `(401, ...)` if the token is missing/empty or doesn't match.
/// Returns `(503, ...)` if the DB pool is exhausted. Returns `(500, ...)`
/// if the machine row can't be read.
///
/// Backward-compat: machines created before migration 002 have no stored
/// hash; for those, any non-empty token is accepted with a warning (same
/// behaviour as the other `:machine_id` routes).
async fn verify_connect_token(
    state: &AppState,
    machine_id: &str,
    query: &PtyQuery,
) -> Result<String, (StatusCode, String)> {
    let token = query.token.as_deref().filter(|t| !t.is_empty()).ok_or((
        StatusCode::UNAUTHORIZED,
        "missing or empty connect token".to_string(),
    ))?;

    let token_hash = {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        hex::encode(hasher.finalize())
    };

    let conn = state.db.get().map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("db pool exhausted: {e}"),
        )
    })?;

    let stored_hash: Option<String> = conn
        .query_row(
            "SELECT connect_token_hash FROM machines WHERE id = ?1 AND status = 'active'",
            rusqlite::params![machine_id],
            |row| row.get(0),
        )
        .ok();

    match stored_hash {
        Some(h) if h == token_hash => {
            tracing::info!(machine = %machine_id, "messages: connect_token verified");
        }
        Some(_) => {
            tracing::warn!(
                machine = %machine_id,
                "messages: connect_token mismatch (HTTP 401)"
            );
            return Err((
                StatusCode::UNAUTHORIZED,
                "invalid connect token".to_string(),
            ));
        }
        None => {
            // Backward-compat: machines created before migration 002 have
            // no stored hash. Accept with a warning, matching pty.rs /
            // exec.rs / git.rs.
            tracing::warn!(
                machine = %machine_id,
                "messages: no connect_token_hash stored — accepting with warning"
            );
        }
    }

    let tenant_id: String = conn
        .query_row(
            "SELECT tenant_id FROM machines WHERE id = ?1",
            rusqlite::params![machine_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "unknown".to_string());

    // Release the pooled connection before the (potentially long) stream.
    drop(conn);
    Ok(tenant_id)
}

/// Fetch all messages newer than `last_id` for the given machine/channel.
///
/// Used by [`stream_messages`] on each 500ms tick. Returns rows ordered
/// oldest-first so the `last_id` watermark advances monotonically.
///
/// The query matches the same recipient rule as [`poll_messages`]:
/// `(to_machine = ? OR to_machine IS NULL) AND channel = ?`.
fn fetch_new_messages(
    db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    machine_id: &str,
    channel: &str,
    last_id: i64,
) -> anyhow::Result<Vec<Message>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, from_machine, to_machine, channel, body, created_at
         FROM agent_messages
         WHERE (to_machine = ?1 OR to_machine IS NULL)
           AND channel = ?2
           AND id > ?3
         ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![machine_id, channel, last_id], |row| {
            let id: i64 = row.get(0)?;
            let from_machine: String = row.get(1)?;
            let to_machine: Option<String> = row.get(2)?;
            let channel: String = row.get(3)?;
            let body_str: String = row.get(4)?;
            let created_at: String = row.get(5)?;
            let body: serde_json::Value =
                serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
            Ok(Message {
                id,
                from_machine,
                to_machine,
                channel,
                body,
                created_at,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- PostMessageRequest -------------------------------------------------

    #[test]
    fn test_post_message_request_deserialize_directed() {
        let json = r#"{
            "to": "machine_b",
            "channel": "tasks.coord",
            "body": {"step": 1, "status": "ok"}
        }"#;
        let req: PostMessageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.to.as_deref(), Some("machine_b"));
        assert_eq!(req.channel, "tasks.coord");
        assert_eq!(req.body["step"], 1);
        assert_eq!(req.body["status"], "ok");
    }

    #[test]
    fn test_post_message_request_deserialize_broadcast_explicit_null() {
        // `to: null` → broadcast.
        let json = r#"{
            "to": null,
            "channel": "build-events",
            "body": "build started"
        }"#;
        let req: PostMessageRequest = serde_json::from_str(json).unwrap();
        assert!(req.to.is_none());
        assert_eq!(req.channel, "build-events");
        assert_eq!(
            req.body,
            serde_json::Value::String("build started".to_string())
        );
    }

    #[test]
    fn test_post_message_request_deserialize_broadcast_omitted_to() {
        // Omitting `to` is also valid (serde defaults Option to None).
        let json = r#"{
            "channel": "heartbeat",
            "body": {"ts": 1700000000}
        }"#;
        let req: PostMessageRequest = serde_json::from_str(json).unwrap();
        assert!(req.to.is_none());
        assert_eq!(req.channel, "heartbeat");
        assert_eq!(req.body["ts"], 1700000000);
    }

    #[test]
    fn test_post_message_request_body_array_round_trip() {
        // The `body` field accepts any JSON value, including arrays.
        let json = r#"{
            "channel": "log",
            "body": [1, 2, 3, {"nested": true}]
        }"#;
        let req: PostMessageRequest = serde_json::from_str(json).unwrap();
        assert!(req.body.is_array());
        assert_eq!(req.body.as_array().unwrap().len(), 4);
        assert_eq!(req.body[3]["nested"], true);
    }

    #[test]
    fn test_post_message_request_body_null_round_trip() {
        // `body: null` is technically valid JSON; it deserializes to Value::Null.
        let json = r#"{
            "channel": "ping",
            "body": null
        }"#;
        let req: PostMessageRequest = serde_json::from_str(json).unwrap();
        assert!(req.body.is_null());
        assert_eq!(req.channel, "ping");
    }

    #[test]
    fn test_post_message_request_missing_channel_fails() {
        // `channel` is required — its absence must error.
        let json = r#"{
            "to": "machine_b",
            "body": "hello"
        }"#;
        let res: Result<PostMessageRequest, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn test_post_message_request_missing_body_fails() {
        // `body` is required — its absence must error.
        let json = r#"{
            "to": "machine_b",
            "channel": "ch"
        }"#;
        let res: Result<PostMessageRequest, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    // --- PostMessageResponse ------------------------------------------------

    #[test]
    fn test_post_message_response_serialize() {
        let resp = PostMessageResponse {
            id: 42,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"created_at\":\"2026-01-01T00:00:00Z\""));
        // The serialized value must be a JSON object with exactly 2 keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn test_post_message_response_serialize_zero_id() {
        // Edge: id 0 (would only happen if last_insert_rowid returned 0,
        // i.e. no insert succeeded — but the type still serializes fine).
        let resp = PostMessageResponse {
            id: 0,
            created_at: "2026-01-01 00:00:00".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":0"));
        assert!(json.contains("\"created_at\":\"2026-01-01 00:00:00\""));
    }

    // --- Message (poll response item / SSE payload) ------------------------

    #[test]
    fn test_message_serialize_directed() {
        let msg = Message {
            id: 7,
            from_machine: "machine_a".to_string(),
            to_machine: Some("machine_b".to_string()),
            channel: "tasks.coord".to_string(),
            body: serde_json::json!({"step": 1}),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"id\":7"));
        assert!(json.contains("\"from_machine\":\"machine_a\""));
        assert!(json.contains("\"to_machine\":\"machine_b\""));
        assert!(json.contains("\"channel\":\"tasks.coord\""));
        assert!(json.contains("\"created_at\":\"2026-01-01T00:00:00Z\""));
        assert!(json.contains("\"step\":1"));
        // The serialized value must be a JSON object with exactly 6 keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 6);
    }

    #[test]
    fn test_message_serialize_broadcast() {
        // Broadcast message: `to_machine` is null.
        let msg = Message {
            id: 99,
            from_machine: "machine_a".to_string(),
            to_machine: None,
            channel: "build-events".to_string(),
            body: serde_json::json!({"event": "started"}),
            created_at: "2026-01-02T03:04:05Z".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"id\":99"));
        assert!(json.contains("\"to_machine\":null"));
        assert!(json.contains("\"channel\":\"build-events\""));
        assert!(json.contains("\"event\":\"started\""));
    }

    #[test]
    fn test_message_serialize_complex_body() {
        // Body can be any JSON: nested objects, arrays, mixed.
        let msg = Message {
            id: 1234,
            from_machine: "m1".to_string(),
            to_machine: Some("m2".to_string()),
            channel: "ch".to_string(),
            body: serde_json::json!({
                "artifacts": [{"path": "/out/a"}, {"path": "/out/b"}],
                "metrics": {"cpu_secs": 12.5, "mem_mb": 256},
                "ok": true
            }),
            created_at: "2026-01-01 00:00:00".to_string(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(v["body"]["artifacts"].as_array().unwrap().len(), 2);
        assert_eq!(v["body"]["metrics"]["cpu_secs"], 12.5);
        assert_eq!(v["body"]["ok"], true);
    }

    // --- MessagesPollQuery --------------------------------------------------

    #[test]
    fn test_messages_poll_query_deserialize_full() {
        // Build via a JSON Value (the Deserialize derive is shared between
        // serde_json and serde_urlencoded, so this exercises the same
        // field mappings axum's Query extractor relies on).
        let v = serde_json::json!({
            "token": "abc123",
            "channel": "build-events",
            "since": "2026-01-01T00:00:00Z"
        });
        let q: MessagesPollQuery = serde_json::from_value(v).unwrap();
        assert_eq!(q.token.as_deref(), Some("abc123"));
        assert_eq!(q.channel, "build-events");
        assert_eq!(q.since, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn test_messages_poll_query_deserialize_no_token() {
        // `token` is optional — omitted means unauthenticated (the handler
        // will reject with 401), but the deserialize must succeed.
        let v = serde_json::json!({
            "channel": "ch",
            "since": "2026-01-01T00:00:00Z"
        });
        let q: MessagesPollQuery = serde_json::from_value(v).unwrap();
        assert!(q.token.is_none());
        assert_eq!(q.channel, "ch");
        assert_eq!(q.since, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn test_messages_poll_query_missing_channel_fails() {
        let v = serde_json::json!({"token": "tok", "since": "2026-01-01"});
        let res: Result<MessagesPollQuery, _> = serde_json::from_value(v);
        assert!(res.is_err());
    }

    #[test]
    fn test_messages_poll_query_missing_since_fails() {
        let v = serde_json::json!({"token": "tok", "channel": "ch"});
        let res: Result<MessagesPollQuery, _> = serde_json::from_value(v);
        assert!(res.is_err());
    }

    // --- MessagesStreamQuery ------------------------------------------------

    #[test]
    fn test_messages_stream_query_deserialize() {
        let v = serde_json::json!({
            "token": "tok",
            "channel": "build-events"
        });
        let q: MessagesStreamQuery = serde_json::from_value(v).unwrap();
        assert_eq!(q.token.as_deref(), Some("tok"));
        assert_eq!(q.channel, "build-events");
    }

    #[test]
    fn test_messages_stream_query_missing_channel_fails() {
        // `channel` is required for the stream — missing it must error.
        let v = serde_json::json!({"token": "tok"});
        let res: Result<MessagesStreamQuery, _> = serde_json::from_value(v);
        assert!(res.is_err());
    }

    #[test]
    fn test_messages_stream_query_missing_token_ok() {
        // `token` is optional at the deserialization layer (the handler
        // will reject with 401 if missing — but deserialize succeeds).
        let v = serde_json::json!({"channel": "build-events"});
        let q: MessagesStreamQuery = serde_json::from_value(v).unwrap();
        assert!(q.token.is_none());
        assert_eq!(q.channel, "build-events");
    }
}
