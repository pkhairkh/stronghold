//! Session manager — create, approve, deny, revoke, extend sessions.

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use std::time::Duration;
use tokio::time::timeout;

use crate::routes::agent::{ExtendRequest, OrderRequest, OrderResponse};
use crate::routes::AppState;

/// Error types for session operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session not found")]
    NotFound,
    #[error("session expired")]
    Expired,
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("pool error: {0}")]
    Pool(#[from] r2d2::Error),
}

/// The decision returned from `wait_for_decision`.
pub enum Decision {
    Approved,
    Denied,
    Timeout,
}

/// Create a pending session (before phone approval).
pub fn create_pending(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    req: &OrderRequest,
) -> Result<String> {
    let session_id = format!("sess_{}", ulid::Ulid::new());
    let conn = db.get()?;

    conn.execute(
        "INSERT INTO pending_sessions
         (id, tenant_id, image, ttl_secs, reason, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', datetime('now'))",
        params![session_id, tenant_id, req.image, req.ttl_secs, req.reason],
    )?;

    Ok(session_id)
}

/// Wait for the tenant's phone decision (long-poll, 60s timeout).
pub async fn wait_for_decision(
    db: &Pool<SqliteConnectionManager>,
    session_id: &str,
    timeout_secs: u64,
) -> Result<Decision> {
    let deadline = Duration::from_secs(timeout_secs);

    let result: Result<Result<Decision, anyhow::Error>, _> = timeout(deadline, async {
        loop {
            let decision = check_decision(db, session_id)?;
            if let Some(d) = decision {
                return Ok(d);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;

    match result {
        Ok(Ok(d)) => Ok(d),
        Ok(Err(e)) => Err(anyhow::anyhow!("Database error: {}", e)),
        Err(_) => Ok(Decision::Timeout),
    }
}

fn check_decision(
    db: &Pool<SqliteConnectionManager>,
    session_id: &str,
) -> Result<Option<Decision>> {
    let conn = db.get()?;
    let status: String = conn.query_row(
        "SELECT status FROM pending_sessions WHERE id = ?1",
        params![session_id],
        |row| row.get(0),
    )?;

    match status.as_str() {
        "approved" => Ok(Some(Decision::Approved)),
        "denied" => Ok(Some(Decision::Denied)),
        _ => Ok(None),
    }
}

/// Mark a session as approved (called after WebAuthn verification).
pub fn approve_session(db: &Pool<SqliteConnectionManager>, session_id: &str) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE pending_sessions SET status = 'approved', decided_at = datetime('now') WHERE id = ?1",
        params![session_id],
    )?;
    Ok(())
}

/// Mark a session as denied.
pub fn deny_session(db: &Pool<SqliteConnectionManager>, session_id: &str) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE pending_sessions SET status = 'denied', decided_at = datetime('now') WHERE id = ?1",
        params![session_id],
    )?;
    Ok(())
}

/// Finalize an approved session — schedule the pod and return the connect token.
pub async fn finalize_session(
    state: &AppState,
    tenant_id: &str,
    session_id: &str,
    req: &OrderRequest,
) -> Result<OrderResponse> {
    // Schedule the pod on a worker
    let machine = crate::machines::scheduler::schedule(state, tenant_id, req).await?;

    // Generate connect token
    let connect_token = format!("stronghold_sess_{}", ulid::Ulid::new());

    // Hash the connect token for storage (never store the plaintext token)
    use sha2::{Digest, Sha256};
    let connect_token_hash = {
        let mut hasher = Sha256::new();
        hasher.update(connect_token.as_bytes());
        hex::encode(hasher.finalize())
    };

    // Record in machines table
    let conn = state.db.get()?;
    conn.execute(
        "INSERT INTO machines
         (id, tenant_id, image, worker, status, cpu, memory_gb, connect_token_hash, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, datetime('now'),
                 datetime('now', '+' || ?8 || ' seconds'))",
        params![
            machine.id,
            tenant_id,
            req.image,
            machine.worker,
            req.compute.cpu.unwrap_or(4),
            req.compute.memory_gb.unwrap_or(8),
            connect_token_hash,
            req.ttl_secs,
        ],
    )?;

    // Log to audit
    crate::audit::log::entry(
        &state.db,
        tenant_id,
        &machine.id,
        "session_started",
        serde_json::json!({
            "session_id": session_id,
            "image": req.image,
            "ttl_secs": req.ttl_secs,
            "reason": req.reason,
        }),
        &state.audit_keys,
    )?;

    Ok(OrderResponse {
        machine_id: machine.id.clone(),
        connect_token,
        expires_at: chrono::Utc::now()
            .checked_add_signed(chrono::Duration::seconds(req.ttl_secs as i64))
            .unwrap()
            .to_rfc3339(),
        worker: machine.worker.clone(),
        worker_sev_snp_attested: machine.sev_snp_attested,
        pty_endpoint: format!("/agent/{}/pty", machine.id),
        audit_stream: format!("/agent/{}/audit", machine.id),
    })
}

/// Resume an existing session (no phone approval needed).
pub fn resume_session(
    state: &AppState,
    tenant_id: &str,
    machine_id: &str,
) -> Result<OrderResponse> {
    let conn = state.db.get()?;

    let machine: (String, String, String, String, String) = conn
        .query_row(
            "SELECT id, image, worker, expires_at,
                CASE WHEN worker_sev_snp = 1 THEN 'true' ELSE 'false' END
         FROM machines
         WHERE id = ?1 AND tenant_id = ?2 AND status = 'active'",
            params![machine_id, tenant_id],
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
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => SessionError::NotFound.into(),
            _ => anyhow::Error::from(SessionError::from(e)),
        })?;

    // Check expiry
    let expires_at =
        chrono::DateTime::parse_from_rfc3339(&machine.3).map_err(|_| SessionError::Expired)?;
    if chrono::Utc::now() > expires_at.with_timezone(&chrono::Utc) {
        return Err(SessionError::Expired.into());
    }

    let connect_token = format!("stronghold_sess_{}", ulid::Ulid::new());

    Ok(OrderResponse {
        machine_id: machine.0,
        connect_token,
        expires_at: machine.3,
        worker: machine.2,
        worker_sev_snp_attested: machine.4 == "true",
        pty_endpoint: format!("/agent/{}/pty", machine_id),
        audit_stream: format!("/agent/{}/audit", machine_id),
    })
}

/// Release (kill) a session early.
pub async fn release_session(state: &AppState, tenant_id: &str, machine_id: &str) -> Result<()> {
    // Kill the pod
    crate::machines::scheduler::kill_pod(state, machine_id).await?;

    // Update database
    let conn = state.db.get()?;
    conn.execute(
        "UPDATE machines SET status = 'released', killed_at = datetime('now')
         WHERE id = ?1 AND tenant_id = ?2",
        params![machine_id, tenant_id],
    )?;

    // Audit log
    crate::audit::log::entry(
        &state.db,
        tenant_id,
        machine_id,
        "session_released",
        serde_json::json!({"reason": "agent_released"}),
        &state.audit_keys,
    )?;

    Ok(())
}

/// Revoke a session (instant kill, from phone).
pub async fn revoke_session(state: &AppState, tenant_id: &str, machine_id: &str) -> Result<()> {
    // Kill the pod
    crate::machines::scheduler::kill_pod(state, machine_id).await?;

    // Update database
    let conn = state.db.get()?;
    conn.execute(
        "UPDATE machines SET status = 'revoked', killed_at = datetime('now')
         WHERE id = ?1 AND tenant_id = ?2",
        params![machine_id, tenant_id],
    )?;

    // Audit log
    crate::audit::log::entry(
        &state.db,
        tenant_id,
        machine_id,
        "session_revoked",
        serde_json::json!({"reason": "phone_revoked"}),
        &state.audit_keys,
    )?;

    Ok(())
}

/// Create an extend request (triggers phone approval).
pub fn create_extend_request(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    req: &ExtendRequest,
) -> Result<String> {
    let session_id = format!("ext_{}", ulid::Ulid::new());
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO pending_sessions
         (id, tenant_id, machine_id, ttl_secs, reason, status, created_at, is_extend)
         VALUES (?1, ?2, ?3, ?4, 'extend session', 'pending', datetime('now'), 1)",
        params![session_id, tenant_id, req.machine_id, req.additional_secs],
    )?;
    Ok(session_id)
}

/// Finalize an approved extension.
pub async fn finalize_extend(
    state: &AppState,
    tenant_id: &str,
    session_id: &str,
    req: &ExtendRequest,
) -> Result<OrderResponse> {
    let conn = state.db.get()?;

    // Extend the machine's TTL
    conn.execute(
        "UPDATE machines
         SET expires_at = datetime(expires_at, '+' || ?1 || ' seconds')
         WHERE id = ?2 AND tenant_id = ?3",
        params![req.additional_secs, req.machine_id, tenant_id],
    )?;

    // Audit log
    crate::audit::log::entry(
        &state.db,
        tenant_id,
        &req.machine_id,
        "session_extended",
        serde_json::json!({
            "session_id": session_id,
            "additional_secs": req.additional_secs,
        }),
        &state.audit_keys,
    )?;

    // Return the updated session info
    resume_session(state, tenant_id, &req.machine_id)
}

/// SSE stream of pending approval requests for a tenant.
///
/// Every 500ms, polls the `pending_sessions` table for rows with
/// `status = 'pending'` and the given `tenant_id`. Sessions not previously
/// yielded on this stream are emitted as `approval_request` SSE events with
/// a JSON payload of:
/// `{"request_id":"...","image":"...","ttl_secs":...,"reason":"...","created_at":"..."}`
///
/// A `heartbeat` data event is emitted every 30s as a keepalive between
/// approval requests so the phone's watchdog (45s timeout) doesn't drop the
/// connection during quiet periods.
pub fn pending_approval_stream(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    use std::collections::HashSet;

    // Clone into owned values so the returned stream is `'static` and not
    // tied to the caller's borrowed references (the pool is cheaply cloneable
    // — it's just an `Arc` around the shared connection pool).
    let db = db.clone();
    let tenant_id = tenant_id.to_string();

    async_stream::stream! {
        // Tracks session IDs already yielded on this stream so we only emit
        // each pending request once per connection.
        let mut seen: HashSet<String> = HashSet::new();

        let mut poll_interval = tokio::time::interval(Duration::from_millis(500));
        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(30));
        // Delay (rather than burst) if a tick is missed while we were busy
        // yielding approval_request events — keeps the cadence steady.
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Discard the immediate first tick so the first poll happens after
        // 500ms and the first heartbeat after 30s.
        poll_interval.tick().await;
        heartbeat_interval.tick().await;

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    // Query pending_sessions for this tenant.
                    //
                    // Wrapped in a synchronous closure so all DB borrows are
                    // released before we yield (the pooled connection is
                    // returned to the pool once the closure exits).
                    #[allow(clippy::type_complexity)]
                    let rows_result = (|| -> anyhow::Result<Vec<(
                        String,
                        Option<String>,
                        Option<i64>,
                        Option<String>,
                        String,
                    )>> {
                        let conn = db.get()?;
                        let mut stmt = conn.prepare(
                            "SELECT id, image, ttl_secs, reason, created_at
                             FROM pending_sessions
                             WHERE tenant_id = ?1 AND status = 'pending'",
                        )?;
                        let rows = stmt
                            .query_map(params![&tenant_id], |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, Option<String>>(1)?,
                                    row.get::<_, Option<i64>>(2)?,
                                    row.get::<_, Option<String>>(3)?,
                                    row.get::<_, String>(4)?,
                                ))
                            })?
                            .filter_map(|r| r.ok())
                            .collect();
                        Ok(rows)
                    })();

                    match rows_result {
                        Ok(rows) => {
                            for (id, image, ttl_secs, reason, created_at) in rows {
                                // `insert` returns true if the id was NOT
                                // already present — i.e. this is a new
                                // pending request we haven't yielded yet.
                                if seen.insert(id.clone()) {
                                    let payload = serde_json::json!({
                                        "request_id": id,
                                        "image": image.unwrap_or_default(),
                                        "ttl_secs": ttl_secs.unwrap_or(0),
                                        "reason": reason.unwrap_or_default(),
                                        "created_at": created_at,
                                    });
                                    yield Ok(axum::response::sse::Event::default()
                                        .event("approval_request")
                                        .data(payload.to_string()));
                                }
                            }
                        }
                        Err(e) => {
                            // Transient DB error — log and keep the stream
                            // alive; the next poll tick will retry.
                            tracing::error!(
                                error = %e,
                                tenant = %tenant_id,
                                "Failed to poll pending_sessions for SSE stream"
                            );
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    yield Ok(axum::response::sse::Event::default().data("heartbeat"));
                }
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_memory_pool;
    use crate::routes::agent::{ComputeRequest, OrderRequest};
    use crate::tenants::registry;

    fn make_order_req() -> OrderRequest {
        OrderRequest {
            image: "stronghold/rust-nightly:2026.07".to_string(),
            ttl_secs: 3600,
            reason: "test".to_string(),
            compute: ComputeRequest {
                cpu: Some(4),
                memory_gb: Some(8),
                dedicated: Some(false),
                gpu: Some(false),
            },
            ephemeral_volumes: vec!["~/work".to_string(), "~/.cache".to_string()],
        }
    }

    #[test]
    fn test_create_pending_session() {
        let pool = init_memory_pool().unwrap();
        let tenant = registry::create(&pool, "alice").unwrap();
        let req = make_order_req();
        let session_id = create_pending(&pool, &tenant.id, &req).unwrap();
        assert!(session_id.starts_with("sess_"));

        let conn = pool.get().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM pending_sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn test_approve_session() {
        let pool = init_memory_pool().unwrap();
        let tenant = registry::create(&pool, "alice").unwrap();
        let req = make_order_req();
        let session_id = create_pending(&pool, &tenant.id, &req).unwrap();

        approve_session(&pool, &session_id).unwrap();

        let conn = pool.get().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM pending_sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "approved");
    }

    #[test]
    fn test_deny_session() {
        let pool = init_memory_pool().unwrap();
        let tenant = registry::create(&pool, "alice").unwrap();
        let req = make_order_req();
        let session_id = create_pending(&pool, &tenant.id, &req).unwrap();

        deny_session(&pool, &session_id).unwrap();

        let conn = pool.get().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM pending_sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "denied");
    }

    #[tokio::test]
    async fn test_wait_for_decision_timeout() {
        let pool = init_memory_pool().unwrap();
        let tenant = registry::create(&pool, "alice").unwrap();
        let req = make_order_req();
        let session_id = create_pending(&pool, &tenant.id, &req).unwrap();

        let decision = wait_for_decision(&pool, &session_id, 1).await.unwrap();
        assert!(matches!(decision, Decision::Timeout));
    }

    #[tokio::test]
    async fn test_wait_for_decision_approved() {
        let pool = init_memory_pool().unwrap();
        let tenant = registry::create(&pool, "alice").unwrap();
        let req = make_order_req();
        let session_id = create_pending(&pool, &tenant.id, &req).unwrap();

        let pool_clone = pool.clone();
        let sid_clone = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            approve_session(&pool_clone, &sid_clone).unwrap();
        });

        let decision = wait_for_decision(&pool, &session_id, 5).await.unwrap();
        assert!(matches!(decision, Decision::Approved));
    }

    #[test]
    fn test_create_extend_request() {
        let pool = init_memory_pool().unwrap();
        let tenant = registry::create(&pool, "alice").unwrap();

        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO machines (id, tenant_id, image, worker, status, cpu, memory_gb, created_at, expires_at)
             VALUES (?1, ?2, 'test-image', 'worker-1', 'active', 4, 8, datetime('now'), datetime('now', '+1 hour'))",
            rusqlite::params!["mach_01HXYZ", tenant.id],
        )
        .unwrap();
        drop(conn);

        let req = ExtendRequest {
            machine_id: "mach_01HXYZ".to_string(),
            additional_secs: 3600,
        };
        let ext_id = create_extend_request(&pool, &tenant.id, &req).unwrap();
        assert!(ext_id.starts_with("ext_"));
    }

    #[test]
    fn test_scopes_default_config() {
        let config = crate::sessions::scopes::ScopeConfig::default();
        assert_eq!(config.scopes.len(), 3);
        assert_eq!(config.scopes[0].name, "default");
        assert_eq!(config.scopes[1].name, "extended");
        assert_eq!(config.scopes[2].name, "destructive");
        assert_eq!(config.scopes[2].require_credentials, 2);
    }

    #[test]
    fn test_matches_deceptive_pattern() {
        let config = crate::sessions::scopes::ScopeConfig::default();
        assert!(crate::sessions::scopes::matches_deceptive_pattern(&config, "rm -rf /").is_some());
        assert!(crate::sessions::scopes::matches_deceptive_pattern(&config, "git push --force").is_some());
        assert!(crate::sessions::scopes::matches_deceptive_pattern(&config, "ls -la").is_none());
    }
}

