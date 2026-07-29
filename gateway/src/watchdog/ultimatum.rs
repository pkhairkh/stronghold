//! Ultimatum injection — watchdog → agent escalation protocol.
//!
//! When the watchdog detects that an agent is off-task, spinning, or taking
//! shortcuts, it issues an *ultimatum*. Ultimata come in three escalating
//! levels:
//!
//! | Level | Variant      | Persisted? | Bus post? | Phone push? |
//! |-------|--------------|------------|-----------|-------------|
//! | 1     | `Warning`    | no         | no        | no          |
//! | 2     | `Directive`  | yes        | no        | no          |
//! | 3     | `Escalation` | yes        | yes       | yes         |
//!
//! - **Level 1 (Warning)** — a soft nudge injected into the agent's PTY as a
//!   structured control message (OSC escape sequence, same envelope as the
//!   mid-session reprompt in `instruct.rs`). Nothing is persisted; the
//!   warning is fire-and-forget.
//! - **Level 2 (Directive)** — the same PTY injection, plus a row in the
//!   `ultimata` table with `acknowledged = 0`. The agent is expected to
//!   acknowledge by running `echo ACK_TASK_FOCUS`; the acknowledgment is
//!   detected by [`check_ultimatum_acknowledgment`] scanning the audit log.
//! - **Level 3 (Escalation)** — Level 2 plus an escalation message on the
//!   `agent_messages` bus (channel `escalation`, addressed to the planner)
//!   and a best-effort phone push notification. Used when the agent has not
//!   acknowledged a Level 2 directive within the deadline.
//!
//! Implemented in: P4 (this file).
//! Tested by: `ultimatum::tests` (13 unit + integration tests).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::routes::AppState;

/// Sentinel machine ID used by the watchdog when posting ultimata and
/// escalations. Matches the `"from": "watchdog"` convention from the
/// watchdog system prompt (`agent/prompts/watchdog.md`).
const WATCHDOG_MACHINE: &str = "watchdog";

/// Channel name used for Level 3 escalation messages on the agent bus.
const ESCALATION_CHANNEL: &str = "escalation";

/// The recipient of a Level 3 escalation. The watchdog prompt names the
/// planner as the escalation target ("Consider revoking session or
/// re-planning with different approach").
const ESCALATION_RECIPIENT: &str = "planner";

/// Acknowledgment sentinel the agent must echo back. The watchdog detects
/// this string in the audit log to mark an ultimatum as acknowledged.
const ACK_TOKEN: &str = "ACK_TASK_FOCUS";

/// Escalation severity level for an ultimatum.
///
/// Serialized by serde as the bare variant name (`"Warning"`, `"Directive"`,
/// `"Escalation"`) — see the tests for the exact wire format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UltimatumLevel {
    /// Level 1 — soft warning injected via PTY. Not persisted.
    Warning,
    /// Level 2 — directive injected + persisted in `ultimata` with
    /// `acknowledged = 0`. Agent must echo `ACK_TASK_FOCUS` to clear.
    Directive,
    /// Level 3 — escalation: Level 2 + agent_messages bus post + phone push.
    Escalation,
}

impl UltimatumLevel {
    /// Map a level to its integer code for DB storage.
    pub fn as_int(&self) -> i64 {
        match self {
            UltimatumLevel::Warning => 1,
            UltimatumLevel::Directive => 2,
            UltimatumLevel::Escalation => 3,
        }
    }
}

/// An ultimatum to be issued to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ultimatum {
    pub level: UltimatumLevel,
    pub target_machine: String,
    pub target_task_id: Option<String>,
    pub message: String,
    pub deadline_seconds: Option<u64>,
}

/// Issue an ultimatum to an agent.
///
/// See the module docs for the per-level behaviour. The PTY injection is
/// best-effort: if the target machine has no active PTY session, the
/// ultimatum is still persisted (for Level 2/3) and escalated (for Level 3).
/// The phone push for Level 3 is also best-effort and silently skipped when
/// the target machine has no tenant association (e.g. the machine row was
/// already cleaned up).
///
/// # Panics / errors
///
/// Returns `Err` only if the DB pool is exhausted or the `ultimata` insert
/// itself fails — at which point the ultimatum has not been recorded and the
/// caller should retry. All other side effects (PTY delivery, message-bus
/// post, phone push) are best-effort and log on failure.
pub async fn issue_ultimatum(state: &AppState, ultimatum: &Ultimatum) -> Result<()> {
    // 1. Build the control message JSON — same envelope shape the watchdog
    //    system prompt documents for Level 1/2 ultimata.
    let control_msg = serde_json::json!({
        "type": "ultimatum",
        "level": ultimatum.level.as_int(),
        "to": ultimatum.target_machine,
        "task_id": ultimatum.target_task_id,
        "message": ultimatum.message,
        "deadline_seconds": ultimatum.deadline_seconds,
    });

    // 2-3. Look up the PTY registry for target_machine and send the control
    //      message. We use the same OSC escape envelope as the mid-session
    //      reprompt (`instruct.rs` control mode) so the agent SDK can parse
    //      both with one code path. Failure to deliver (no session, closed
    //      channel) is logged but non-fatal — the ultimatum is still
    //      recorded and (for Level 3) escalated.
    //
    //      The `RwLock` read guard and the `mpsc::Sender` borrow are both
    //      dropped before any DB access below, so no connection is held
    //      across an `.await`.
    {
        let registry = state.pty_registry.read().await;
        if let Some(sender) = registry.get(&ultimatum.target_machine) {
            let envelope = format!(
                "\x1b]51;stronghold:control\x07{}\x1b]51;stronghold:control\x07",
                control_msg
            );
            match sender.send(envelope.into_bytes()).await {
                Ok(_) => tracing::info!(
                    target = %ultimatum.target_machine,
                    level = ultimatum.level.as_int(),
                    "Ultimatum control message injected via PTY"
                ),
                Err(e) => tracing::warn!(
                    target = %ultimatum.target_machine,
                    level = ultimatum.level.as_int(),
                    error = %e,
                    "Failed to inject ultimatum via PTY (session may be closed)"
                ),
            }
        } else {
            tracing::warn!(
                target = %ultimatum.target_machine,
                level = ultimatum.level.as_int(),
                "No active PTY session for ultimatum target — falling through to storage"
            );
        }
    }

    // 4. Persist in the `ultimata` table for Level 2 (Directive) and Level 3
    //    (Escalation). Level 1 (Warning) is fire-and-forget — no DB row.
    //    `acknowledged` defaults to 0 per the schema; we set it explicitly
    //    so the intent is obvious in the INSERT.
    if ultimatum.level != UltimatumLevel::Warning {
        let conn = state.db.get()?;
        conn.execute(
            "INSERT INTO ultimata
             (watchdog_machine, target_machine, target_task_id, level,
              message, acknowledged, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, datetime('now'))",
            rusqlite::params![
                WATCHDOG_MACHINE,
                ultimatum.target_machine,
                ultimatum.target_task_id,
                ultimatum.level.as_int(),
                ultimatum.message,
            ],
        )?;
        let ultimatum_id = conn.last_insert_rowid();
        tracing::info!(
            ultimatum_id = ultimatum_id,
            level = ultimatum.level.as_int(),
            target = %ultimatum.target_machine,
            "Ultimatum persisted in DB with acknowledged=0"
        );
    } else {
        tracing::info!(
            target = %ultimatum.target_machine,
            "Level 1 warning issued (not persisted)"
        );
    }

    // 5. Level 3 escalation: post on the agent message bus + phone push.
    //    The DB connection from step 4 has been dropped by this point, so
    //    `escalate` is free to acquire its own without holding two slots.
    if ultimatum.level == UltimatumLevel::Escalation {
        escalate(state, ultimatum).await;
    }

    Ok(())
}

/// Level 3 escalation helper — posts an escalation message on the agent bus
/// and triggers a best-effort phone push.
///
/// Both side effects are best-effort: a failure to post on the bus or push
/// to the phone does NOT fail the overall `issue_ultimatum` call (the
/// ultimatum is already persisted in the DB at this point).
async fn escalate(state: &AppState, ultimatum: &Ultimatum) {
    // 5a. Post the escalation on the agent_messages bus (channel "escalation").
    //     The body matches the Level 3 escalation shape from the watchdog
    //     system prompt.
    let escalation_body = serde_json::json!({
        "type": "escalation",
        "from": WATCHDOG_MACHINE,
        "to": ESCALATION_RECIPIENT,
        "watched_machine": ultimatum.target_machine,
        "task_id": ultimatum.target_task_id,
        "reason": ultimatum.message,
        "recommendation": "Consider revoking session or re-planning with different approach.",
    });

    if let Ok(conn) = state.db.get() {
        match conn.execute(
            "INSERT INTO agent_messages
             (from_machine, to_machine, channel, body, created_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            rusqlite::params![
                WATCHDOG_MACHINE,
                ESCALATION_RECIPIENT,
                ESCALATION_CHANNEL,
                escalation_body.to_string(),
            ],
        ) {
            Ok(_) => tracing::info!(
                target = %ultimatum.target_machine,
                channel = ESCALATION_CHANNEL,
                "Level 3 escalation posted on agent message bus"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "Failed to post Level 3 escalation on message bus (non-fatal)"
            ),
        }
    } else {
        tracing::warn!("DB pool exhausted — cannot post Level 3 escalation on message bus");
    }

    // 5b. Phone push — best-effort. Look up the tenant via the target
    //     machine's row so we can route the push to the right ntfy topic.
    //     If the machine has already been cleaned up (no row), skip the push.
    let tenant_id: Option<String> = state.db.get().ok().and_then(|conn| {
        conn.query_row(
            "SELECT tenant_id FROM machines WHERE id = ?1",
            rusqlite::params![ultimatum.target_machine],
            |row| row.get(0),
        )
        .ok()
    });

    if let Some(tenant) = tenant_id {
        let push_msg = format!(
            "ESCALATION: agent {} is unresponsive to ultimata. Reason: {}",
            ultimatum.target_machine, ultimatum.message
        );
        match crate::push::ntfy::push_anomaly(
            &tenant,
            &ultimatum.target_machine,
            &push_msg,
            &state.db,
        )
        .await
        {
            Ok(_) => tracing::info!(
                tenant = %tenant,
                "Level 3 escalation phone push delivered"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                tenant = %tenant,
                "Phone push for Level 3 escalation failed (non-fatal)"
            ),
        }
    } else {
        tracing::warn!(
            target = %ultimatum.target_machine,
            "Skipping Level 3 phone push: no tenant association for target machine"
        );
    }
}

/// Check whether an ultimatum was acknowledged.
///
/// The agent acknowledges a Level 2/3 ultimatum by running
/// `echo ACK_TASK_FOCUS`. That command — whether issued via the structured
/// `exec` endpoint or typed into the PTY — is recorded in `audit_entries`
/// as a `cmd_exec` event whose JSON payload contains the acknowledgment
/// token. This function counts such audit entries created *after* the
/// ultimatum's `created_at` timestamp.
///
/// Returns `true` if at least one matching audit entry exists, `false`
/// otherwise (including on any DB error — fail-closed so a transient DB
/// blip doesn't silently mark an ultimatum as acknowledged).
pub fn check_ultimatum_acknowledgment(
    db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    ultimatum_id: i64,
) -> bool {
    let conn = match db.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                ultimatum_id = ultimatum_id,
                error = %e,
                "check_ultimatum_acknowledgment: db pool exhausted — returning false"
            );
            return false;
        }
    };

    // Look up the ultimatum to get the target machine and created_at
    // timestamp. We need both to scope the audit log query.
    let row: rusqlite::Result<(String, String)> = conn.query_row(
        "SELECT target_machine, created_at FROM ultimata WHERE id = ?1",
        rusqlite::params![ultimatum_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );

    let (target_machine, created_at) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            tracing::warn!(
                ultimatum_id = ultimatum_id,
                "check_ultimatum_acknowledgment: ultimatum not found — returning false"
            );
            return false;
        }
        Err(e) => {
            tracing::warn!(
                ultimatum_id = ultimatum_id,
                error = %e,
                "check_ultimatum_acknowledgment: failed to load ultimatum — returning false"
            );
            return false;
        }
    };

    // Count audit entries for the target machine where the payload mentions
    // the ACK token, created after the ultimatum. The audit `ts` is RFC3339
    // (e.g. `2024-01-15T12:34:56.789+00:00`) while the ultimatum `created_at`
    // uses SQLite's `datetime('now')` format (`2024-01-15 12:34:56`). Wrapping
    // both in `datetime(...)` normalizes them to a common
    // `YYYY-MM-DD HH:MM:SS` (UTC, second-precision) form for a correct
    // comparison.
    let like_pattern = format!("%{}%", ACK_TOKEN);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_entries
             WHERE machine_id = ?1
               AND event = 'cmd_exec'
               AND payload LIKE ?2
               AND datetime(ts) > datetime(?3)",
            rusqlite::params![target_machine, like_pattern, created_at],
            |row| row.get(0),
        )
        .unwrap_or(0);

    count > 0
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::hybrid_kem::PushKeys;
    use crate::crypto::hybrid_sig::AuditKeys;
    use crate::db::init_memory_pool;
    use rusqlite::params;
    use std::sync::Arc;

    // ---- Serialization (one test per level) ----

    #[test]
    fn test_serialize_ultimatum_level1_warning() {
        let u = Ultimatum {
            level: UltimatumLevel::Warning,
            target_machine: "mach_01".to_string(),
            target_task_id: Some("task_abc".to_string()),
            message: "You appear to be off-task.".to_string(),
            deadline_seconds: None,
        };
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains("\"level\":\"Warning\""), "json was: {json}");
        assert!(json.contains("\"target_machine\":\"mach_01\""));
        assert!(json.contains("\"target_task_id\":\"task_abc\""));

        let back: Ultimatum = serde_json::from_str(&json).unwrap();
        assert_eq!(back.level, UltimatumLevel::Warning);
        assert_eq!(back.target_machine, "mach_01");
        assert_eq!(back.target_task_id.as_deref(), Some("task_abc"));
        assert_eq!(back.message, "You appear to be off-task.");
        assert_eq!(back.deadline_seconds, None);
    }

    #[test]
    fn test_serialize_ultimatum_level2_directive() {
        let u = Ultimatum {
            level: UltimatumLevel::Directive,
            target_machine: "mach_02".to_string(),
            target_task_id: Some("task_def".to_string()),
            message: "Refocus immediately. Run: echo ACK_TASK_FOCUS".to_string(),
            deadline_seconds: Some(120),
        };
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains("\"level\":\"Directive\""), "json was: {json}");
        assert!(json.contains("\"deadline_seconds\":120"));

        let back: Ultimatum = serde_json::from_str(&json).unwrap();
        assert_eq!(back.level, UltimatumLevel::Directive);
        assert_eq!(back.target_machine, "mach_02");
        assert_eq!(back.target_task_id.as_deref(), Some("task_def"));
        assert_eq!(back.deadline_seconds, Some(120));
    }

    #[test]
    fn test_serialize_ultimatum_level3_escalation() {
        let u = Ultimatum {
            level: UltimatumLevel::Escalation,
            target_machine: "mach_03".to_string(),
            target_task_id: None,
            message: "Unresponsive to Level 2 directive.".to_string(),
            deadline_seconds: Some(60),
        };
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains("\"level\":\"Escalation\""), "json was: {json}");
        assert!(json.contains("\"target_task_id\":null"));

        let back: Ultimatum = serde_json::from_str(&json).unwrap();
        assert_eq!(back.level, UltimatumLevel::Escalation);
        assert!(back.target_task_id.is_none());
        assert_eq!(back.deadline_seconds, Some(60));
    }

    #[test]
    fn test_ultimatum_level_as_int_mapping() {
        assert_eq!(UltimatumLevel::Warning.as_int(), 1);
        assert_eq!(UltimatumLevel::Directive.as_int(), 2);
        assert_eq!(UltimatumLevel::Escalation.as_int(), 3);
    }

    // ---- check_ultimatum_acknowledgment ----

    /// Insert a tenant row so the audit_entries FK constraint is satisfied.
    fn seed_tenant(db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>, tenant_id: &str) {
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![
                tenant_id,
                "test",
                chrono::Utc::now().to_rfc3339(),
                "x"
            ],
        )
        .unwrap();
    }

    /// Insert an ultimatum row directly for acknowledgment tests. The
    /// `created_at` is backdated by 1 minute so that audit entries written
    /// "now" are strictly after it (the comparison is second-precision, so
    /// a same-second ACK would otherwise be missed).
    fn seed_ultimatum_backdated(
        db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
        target_machine: &str,
    ) -> i64 {
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO ultimata
             (watchdog_machine, target_machine, target_task_id, level,
              message, acknowledged, created_at)
             VALUES (?1, ?2, NULL, 2, ?3, 0, datetime('now', '-1 minute'))",
            params![WATCHDOG_MACHINE, target_machine, "Refocus on task"],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn test_check_ultimatum_acknowledgment_detects_ack() {
        let db = init_memory_pool().unwrap();
        seed_tenant(&db, "tenant_ack");
        let ult_id = seed_ultimatum_backdated(&db, "mach_ack");

        // Simulate the agent acknowledging: write a cmd_exec audit entry
        // whose payload contains the ACK token.
        let keys = AuditKeys::generate();
        crate::audit::log::entry(
            &db,
            "tenant_ack",
            "mach_ack",
            "cmd_exec",
            serde_json::json!({
                "cmd": "echo",
                "args": [ACK_TOKEN],
                "exit_code": 0,
            }),
            &keys,
        )
        .unwrap();

        assert!(
            check_ultimatum_acknowledgment(&db, ult_id),
            "ultimatum should be acknowledged after ACK_TASK_FOCUS audit entry"
        );
    }

    #[test]
    fn test_check_ultimatum_acknowledgment_without_ack_is_false() {
        let db = init_memory_pool().unwrap();
        seed_tenant(&db, "tenant_noack");
        let ult_id = seed_ultimatum_backdated(&db, "mach_noack");

        // A non-ACK command — should NOT mark the ultimatum as acknowledged.
        let keys = AuditKeys::generate();
        crate::audit::log::entry(
            &db,
            "tenant_noack",
            "mach_noack",
            "cmd_exec",
            serde_json::json!({
                "cmd": "ls",
                "args": ["-la"],
                "exit_code": 0,
            }),
            &keys,
        )
        .unwrap();

        assert!(
            !check_ultimatum_acknowledgment(&db, ult_id),
            "ultimatum should NOT be acknowledged without an ACK audit entry"
        );
    }

    #[test]
    fn test_check_ultimatum_acknowledgment_unknown_id_is_false() {
        let db = init_memory_pool().unwrap();
        assert!(
            !check_ultimatum_acknowledgment(&db, 9999),
            "unknown ultimatum id should return false"
        );
    }

    #[test]
    fn test_check_ultimatum_acknowledgment_ignores_pre_ultimatum_ack() {
        // An ACK that predates the ultimatum must NOT count — only ACKs
        // issued *after* the ultimatum clear it.
        let db = init_memory_pool().unwrap();
        seed_tenant(&db, "tenant_pre");

        // Insert the ultimatum first with created_at = now.
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO ultimata
             (watchdog_machine, target_machine, target_task_id, level,
              message, acknowledged, created_at)
             VALUES (?1, ?2, NULL, 2, ?3, 0, datetime('now'))",
            params![WATCHDOG_MACHINE, "mach_pre", "Refocus"],
        )
        .unwrap();
        let ult_id = conn.last_insert_rowid();
        drop(conn);

        // Write an ACK audit entry at the current time (via audit::log::entry,
        // which stamps `ts = chrono::Utc::now()`).
        let keys = AuditKeys::generate();
        crate::audit::log::entry(
            &db,
            "tenant_pre",
            "mach_pre",
            "cmd_exec",
            serde_json::json!({"cmd": "echo", "args": [ACK_TOKEN]}),
            &keys,
        )
        .unwrap();

        // Now bump the ultimatum's created_at 5 minutes into the future so
        // the ACK predates it.
        let conn = db.get().unwrap();
        conn.execute(
            "UPDATE ultimata SET created_at = datetime('now', '+5 minutes')
             WHERE id = ?1",
            params![ult_id],
        )
        .unwrap();

        assert!(
            !check_ultimatum_acknowledgment(&db, ult_id),
            "ACK that predates the ultimatum should NOT count"
        );
    }

    // ---- issue_ultimatum integration ----

    /// Build an AppState backed by an in-memory DB for testing.
    fn setup_state() -> AppState {
        let pool = init_memory_pool().unwrap();
        AppState {
            db: pool,
            audit_keys: AuditKeys::generate(),
            push_keys: PushKeys::generate(),
            pty_registry: Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    fn count_ultimata(db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>) -> i64 {
        let conn = db.get().unwrap();
        conn.query_row("SELECT COUNT(*) FROM ultimata", [], |row| row.get(0))
            .unwrap()
    }

    fn count_escalations(db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>) -> i64 {
        let conn = db.get().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM agent_messages WHERE channel = ?1",
            params![ESCALATION_CHANNEL],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_issue_ultimatum_level1_no_db_row() {
        let state = setup_state();
        assert_eq!(count_ultimata(&state.db), 0);

        let ultimatum = Ultimatum {
            level: UltimatumLevel::Warning,
            target_machine: "mach_l1".to_string(),
            target_task_id: None,
            message: "Heads up — you're drifting.".to_string(),
            deadline_seconds: None,
        };
        issue_ultimatum(&state, &ultimatum).await.unwrap();

        // Level 1: no DB row, no escalation on the bus.
        assert_eq!(count_ultimata(&state.db), 0, "Level 1 must not persist");
        assert_eq!(count_escalations(&state.db), 0);
    }

    #[tokio::test]
    async fn test_issue_ultimatum_level2_persists_row() {
        let state = setup_state();

        let ultimatum = Ultimatum {
            level: UltimatumLevel::Directive,
            target_machine: "mach_l2".to_string(),
            target_task_id: Some("task_l2".to_string()),
            message: "Stop and refocus. Acknowledge with: echo ACK_TASK_FOCUS".to_string(),
            deadline_seconds: Some(120),
        };
        issue_ultimatum(&state, &ultimatum).await.unwrap();

        // Level 2: exactly one ultimatum row, acknowledged=0.
        assert_eq!(count_ultimata(&state.db), 1);
        let conn = state.db.get().unwrap();
        let (level, ack, msg, task_id, watchdog): (i64, i64, String, Option<String>, String) = conn
            .query_row(
                "SELECT level, acknowledged, message, target_task_id, watchdog_machine
                 FROM ultimata
                 WHERE target_machine = ?1",
                params!["mach_l2"],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(level, 2);
        assert_eq!(ack, 0, "newly-issued ultimatum must start unacknowledged");
        assert!(msg.contains("ACK_TASK_FOCUS"));
        assert_eq!(task_id.as_deref(), Some("task_l2"));
        assert_eq!(watchdog, WATCHDOG_MACHINE);

        // Level 2: no escalation on the bus (only Level 3 escalates).
        assert_eq!(count_escalations(&state.db), 0);
    }

    #[tokio::test]
    async fn test_issue_ultimatum_level3_posts_escalation() {
        let state = setup_state();

        let ultimatum = Ultimatum {
            level: UltimatumLevel::Escalation,
            target_machine: "mach_l3".to_string(),
            target_task_id: Some("task_l3".to_string()),
            message: "Unresponsive to Level 2 directive.".to_string(),
            deadline_seconds: Some(60),
        };
        issue_ultimatum(&state, &ultimatum).await.unwrap();

        // Level 3: ultimatum row + escalation on the bus.
        assert_eq!(count_ultimata(&state.db), 1);
        assert_eq!(count_escalations(&state.db), 1);

        // Verify the escalation message body and routing.
        let conn = state.db.get().unwrap();
        let (from_machine, to_machine, channel, body): (
            String,
            Option<String>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT from_machine, to_machine, channel, body
                 FROM agent_messages
                 WHERE channel = ?1",
                params![ESCALATION_CHANNEL],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(from_machine, WATCHDOG_MACHINE);
        assert_eq!(to_machine.as_deref(), Some(ESCALATION_RECIPIENT));
        assert_eq!(channel, ESCALATION_CHANNEL);
        assert!(body.contains("\"type\":\"escalation\""));
        assert!(body.contains("mach_l3"));
        assert!(body.contains("task_l3"));
        // No machine row ⇒ no tenant ⇒ phone push skipped (no crash, no hang).
    }

    #[tokio::test]
    async fn test_issue_ultimatum_injects_via_pty() {
        let state = setup_state();

        // Register a fake PTY session for the target machine.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        state
            .pty_registry
            .write()
            .await
            .insert("mach_pty".to_string(), tx);

        let ultimatum = Ultimatum {
            level: UltimatumLevel::Warning,
            target_machine: "mach_pty".to_string(),
            target_task_id: None,
            message: "Drifting off task.".to_string(),
            deadline_seconds: None,
        };
        issue_ultimatum(&state, &ultimatum).await.unwrap();

        // The control message should have been delivered to the channel,
        // wrapped in the OSC envelope (same envelope as instruct.rs control
        // mode).
        let received = rx.recv().await.expect("should receive a PTY message");
        let text = String::from_utf8(received).unwrap();
        assert!(
            text.contains("\x1b]51;stronghold:control\x07"),
            "missing OSC envelope: {text:?}"
        );
        assert!(text.contains("\"type\":\"ultimatum\""));
        assert!(text.contains("\"level\":1"));
        assert!(text.contains("\"to\":\"mach_pty\""));
        assert!(text.contains("Drifting off task."));
    }

    #[tokio::test]
    async fn test_issue_ultimatum_without_pty_session_still_persists() {
        // No PTY session registered — issue_ultimatum should still succeed
        // and persist the ultimatum (Level 2). This mirrors the production
        // scenario where the agent's PTY may have already closed by the time
        // the watchdog escalates.
        let state = setup_state();
        let ultimatum = Ultimatum {
            level: UltimatumLevel::Directive,
            target_machine: "mach_nosession".to_string(),
            target_task_id: None,
            message: "Refocus.".to_string(),
            deadline_seconds: None,
        };
        issue_ultimatum(&state, &ultimatum).await.unwrap();
        assert_eq!(
            count_ultimata(&state.db),
            1,
            "ultimatum must persist even without an active PTY session"
        );
    }
}
