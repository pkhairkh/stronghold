//! Oracle Q&A endpoints.
//!
//! The **Oracle** is a special advisory agent that other agents can query for
//! expert guidance. Questions are submitted via `POST /agent/:machine_id/oracle`
//! and the answer is polled via `GET /agent/:machine_id/oracle/:question_id`.
//!
//! Both questions and answers live on the same `agent_messages` channel
//! (`oracle-<machine_id>`) so any oracle subscriber can pick them up. The
//! `question_id` (a ULID) is embedded in the JSON body of every message on
//! that channel and is used to correlate a reply with its question.
//!
//! # Endpoints
//!
//! | Method | Path                                          | Handler           |
//! |--------|-----------------------------------------------|-------------------|
//! | POST   | `/agent/:machine_id/oracle`                   | [`ask_oracle`]    |
//! | GET    | `/agent/:machine_id/oracle/:question_id`      | [`get_answer`]    |
//!
//! Both endpoints require a valid agent bearer token (tenant-scoped), supplied
//! via the `Authorization: Bearer <token>` header.

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

// ============================================================================
// Request / response types
// ============================================================================

/// Request body for `POST /agent/:machine_id/oracle`.
#[derive(Debug, Deserialize)]
pub struct AskRequest {
    /// The question to ask the oracle, in natural language.
    pub question: String,
    /// Optional free-form context (relevant code snippets, prior outputs,
    /// error messages, etc.) the oracle should consider when answering.
    pub context: Option<serde_json::Value>,
}

/// Response body for `POST /agent/:machine_id/oracle`.
#[derive(Debug, Serialize)]
pub struct AskResponse {
    /// The newly minted question ID (`oq_<ULID>`). Used to poll for the answer.
    pub question_id: String,
    /// Always `"queued"` — the question has been posted and is awaiting an
    /// oracle reply.
    pub status: String,
}

/// Response body for `GET /agent/:machine_id/oracle/:question_id`.
#[derive(Debug, Serialize)]
pub struct AnswerResponse {
    /// The question ID being polled.
    pub question_id: String,
    /// `"answered"` once an oracle reply has been posted, otherwise `"pending"`.
    pub status: String,
    /// The oracle's answer payload. `None` while the status is `"pending"`;
    /// once answered, contains whatever JSON the oracle posted under the
    /// `"answer"` key (typically `{ "text": "..." }`).
    pub answer: Option<serde_json::Value>,
}

// ============================================================================
// Handlers
// ============================================================================

/// Ask the oracle a question.
///
/// Verifies the agent token (tenant-scoped), then inserts a row into
/// `agent_messages` on channel `oracle-<machine_id>` with a JSON body of the
/// form:
///
/// ```json
/// {
///   "question_id": "oq_01HX...",
///   "type": "question",
///   "question": "should I use async or sync I/O here?",
///   "context": { ... },
///   "tenant_id": "tenant_...",
///   "machine_id": "machine_..."
/// }
/// ```
///
/// Returns `{ question_id, status: "queued" }`. The caller polls
/// [`get_answer`] with the returned `question_id` until `status == "answered"`.
pub async fn ask_oracle(
    Path(machine_id): Path<String>,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<AskRequest>,
) -> Result<Json<AskResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let tenant_id = authenticate_agent(&state, &agent_token)?;

    let question_id = format!("oq_{}", ulid::Ulid::new());
    let channel = format!("oracle-{}", machine_id);

    let body = serde_json::json!({
        "question_id": question_id,
        "type": "question",
        "question": req.question,
        "context": req.context,
        "tenant_id": tenant_id,
        "machine_id": machine_id,
    });
    let body_str = body.to_string();

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    conn.execute(
        "INSERT INTO agent_messages (from_machine, to_machine, channel, body, created_at)
         VALUES (?1, NULL, ?2, ?3, datetime('now'))",
        rusqlite::params![&machine_id, &channel, &body_str],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        machine = %machine_id,
        question_id = %question_id,
        channel = %channel,
        "Oracle question queued"
    );

    Ok(Json(AskResponse {
        question_id,
        status: "queued".to_string(),
    }))
}

/// Poll for the oracle's answer to a previously-asked question.
///
/// Scans `agent_messages` on channel `oracle-<machine_id>` for any message
/// whose JSON body contains the given `question_id` and `"type": "answer"`.
/// The most recent such reply (if any) is returned. If no reply has been
/// posted yet, returns `status: "pending"` with a `null` answer.
///
/// The lookup uses a SQL `LIKE` against the serialized JSON body — this is
/// safe because `question_id` is a ULID (ASCII alphanumerics only) and the
/// body is written by this module with serde_json's compact formatting
/// (no spaces around `:`), so the literal substring `"question_id":"<id>"`
/// is stable.
pub async fn get_answer(
    Path((machine_id, question_id)): Path<(String, String)>,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<AnswerResponse>, (StatusCode, String)> {
    let agent_token = extract_token(&headers)?;
    let _tenant_id = authenticate_agent(&state, &agent_token)?;

    let channel = format!("oracle-{}", machine_id);
    // Anchor on the compact-JSON serialization produced by serde_json. ULIDs
    // contain only [0-9A-Z] so no escaping is required.
    let pattern = format!("%\"question_id\":\"{}\"%", question_id);

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db pool exhausted: {e}")))?;

    let mut stmt = conn
        .prepare(
            "SELECT body FROM agent_messages
             WHERE channel = ?1 AND body LIKE ?2
             ORDER BY id DESC",
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows = stmt
        .query_map(rusqlite::params![&channel, &pattern], |row| {
            let body_str: String = row.get(0)?;
            Ok(body_str)
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Walk newest-first; the first row whose body parses as JSON, has
    // `"type": "answer"`, and matches our `question_id` exactly is the
    // oracle's reply.
    for row in rows {
        let body_str = row.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let body: serde_json::Value = match serde_json::from_str(&body_str) {
            Ok(v) => v,
            // Stale / corrupt row — skip; another row may still match.
            Err(_) => continue,
        };

        // Defensive exact-match on question_id (the LIKE pattern is a
        // substring match; this guards against accidental prefix collisions).
        let matches_qid = body
            .get("question_id")
            .and_then(|v| v.as_str())
            .map(|s| s == question_id)
            .unwrap_or(false);
        if !matches_qid {
            continue;
        }

        let is_answer = body
            .get("type")
            .and_then(|v| v.as_str())
            .map(|s| s == "answer")
            .unwrap_or(false);
        if !is_answer {
            continue;
        }

        let answer = body.get("answer").cloned();
        return Ok(Json(AnswerResponse {
            question_id,
            status: "answered".to_string(),
            answer,
        }));
    }

    // No reply yet — keep polling.
    Ok(Json(AnswerResponse {
        question_id,
        status: "pending".to_string(),
        answer: None,
    }))
}

// ============================================================================
// Helpers (mirror routes/tasks.rs — kept private to this module so we don't
// touch any other file)
// ============================================================================

fn extract_token(
    headers: &axum::http::HeaderMap,
) -> Result<String, (StatusCode, String)> {
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

fn authenticate_agent(
    state: &AppState,
    token: &str,
) -> Result<String, (StatusCode, String)> {
    crate::tenants::auth::verify_agent_token(&state.db, token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- AskRequest --------------------------------------------------------

    #[test]
    fn test_ask_request_deserialize_with_context() {
        let json = r#"{
            "question": "should I use async or sync I/O?",
            "context": {"file": "src/main.rs", "lines": [10, 42]}
        }"#;
        let req: AskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.question, "should I use async or sync I/O?");
        assert!(req.context.is_some());
        assert_eq!(req.context.as_ref().unwrap()["file"], "src/main.rs");
        assert_eq!(req.context.as_ref().unwrap()["lines"][1], 42);
    }

    #[test]
    fn test_ask_request_deserialize_minimal() {
        // Context omitted → must default to None.
        let json = r#"{ "question": "what is 2+2?" }"#;
        let req: AskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.question, "what is 2+2?");
        assert!(req.context.is_none());
    }

    #[test]
    fn test_ask_request_rejects_missing_question() {
        // `question` is required; omitting it must fail.
        let json = r#"{ "context": null }"#;
        let result: Result<AskRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // --- AskResponse -------------------------------------------------------

    #[test]
    fn test_ask_response_serialize_queued() {
        let resp = AskResponse {
            question_id: "oq_01HZX9Q8J7ABCDEF".to_string(),
            status: "queued".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"question_id\":\"oq_01HZX9Q8J7ABCDEF\""));
        assert!(json.contains("\"status\":\"queued\""));
        // Exactly 2 keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    // --- AnswerResponse ----------------------------------------------------

    #[test]
    fn test_answer_response_serialize_pending() {
        let resp = AnswerResponse {
            question_id: "oq_01HZX9".to_string(),
            status: "pending".to_string(),
            answer: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"pending\""));
        assert!(json.contains("\"answer\":null"));
    }

    #[test]
    fn test_answer_response_serialize_answered() {
        let resp = AnswerResponse {
            question_id: "oq_01HZX9".to_string(),
            status: "answered".to_string(),
            answer: Some(serde_json::json!({
                "text": "Use async I/O for network-bound workloads.",
                "confidence": 0.92
            })),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"answered\""));
        assert!(json.contains("\"text\":\"Use async I/O for network-bound workloads.\""));
        assert!(json.contains("\"confidence\":0.92"));
        // 3 top-level keys.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn test_answer_response_roundtrip_answered() {
        // Serialize → deserialize → must preserve all fields.
        let original = AnswerResponse {
            question_id: "oq_roundtrip".to_string(),
            status: "answered".to_string(),
            answer: Some(serde_json::json!({"text": "yes"})),
        };
        let json = serde_json::to_string(&original).unwrap();
        // AnswerResponse is Serialize-only by design (it's a response type),
        // but we can deserialize via serde_json::Value to verify round-trip.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["question_id"], "oq_roundtrip");
        assert_eq!(v["status"], "answered");
        assert_eq!(v["answer"]["text"], "yes");
    }
}
