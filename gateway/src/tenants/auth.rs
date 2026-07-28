//! Authentication — agent tokens, phone tokens, WebAuthn credential enrollment.
//!
//! Token types:
//! - **Agent tokens**: minted by the tenant via CLI, scoped per-tenant,
//!   TTL'd (default 24h). Used by agents for ORDER/RESUME/RELEASE/EXTEND.
//! - **Phone tokens**: long-lived, revocable. Stored in the phone's
//!   browser localStorage. Used for SSE and approve/deny/revoke.
//! - **Setup password**: one-time, used only for initial credential enrollment.

use anyhow::Result;
use base64::Engine;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::routes::phone::EnrollRequest;

/// Verify an agent token and return the associated tenant_id.
pub fn verify_agent_token(db: &Pool<SqliteConnectionManager>, token: &str) -> Result<String> {
    let token_hash = hash_token(token);
    let conn = db.get()?;
    let tenant_id: String = conn
        .query_row(
            "SELECT tenant_id FROM agent_tokens
         WHERE token_hash = ?1
           AND (expires_at IS NULL OR expires_at > datetime('now'))
           AND revoked_at IS NULL",
            params![token_hash],
            |row| row.get(0),
        )
        .map_err(|e| anyhow::anyhow!("Invalid or expired agent token: {}", e))?;

    Ok(tenant_id)
}

/// Verify a phone token and return the associated tenant_id.
pub fn verify_phone_token(db: &Pool<SqliteConnectionManager>, token: &str) -> Result<String> {
    let token_hash = hash_token(token);
    let conn = db.get()?;
    let tenant_id: String = conn
        .query_row(
            "SELECT tenant_id FROM phone_tokens
         WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![token_hash],
            |row| row.get(0),
        )
        .map_err(|e| anyhow::anyhow!("Invalid phone token: {}", e))?;

    Ok(tenant_id)
}

/// Verify the one-time setup password.
pub fn verify_setup_password(db: &Pool<SqliteConnectionManager>, password: &str) -> Result<()> {
    let password_hash = hash_token(password);
    let conn = db.get()?;
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tenants
         WHERE setup_password = ?1
           AND setup_used = 0",
        params![password_hash],
        |row| row.get(0),
    )?;

    if exists == 0 {
        return Err(anyhow::anyhow!("Invalid or already-used setup password"));
    }

    Ok(())
}

/// Enroll a new WebAuthn credential.
pub fn enroll_credential(
    db: &Pool<SqliteConnectionManager>,
    req: &EnrollRequest,
) -> Result<String> {
    let conn = db.get()?;

    // Find the tenant by setup password
    let password_hash = hash_token(&req.setup_password);
    let tenant_id: String = conn.query_row(
        "SELECT id FROM tenants WHERE setup_password = ?1 AND setup_used = 0",
        params![password_hash],
        |row| row.get(0),
    )?;

    // Store the credential
    let cred_id = ulid::Ulid::new().to_string();
    conn.execute(
        "INSERT INTO credentials
         (id, tenant_id, credential_id, public_key, aaguid, transports, name, verified, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, datetime('now'))",
        params![
            cred_id,
            tenant_id,
            req.credential_id,
            req.public_key,
            req.aaguid,
            req.transports.join(","),
            req.name,
        ],
    )?;

    // Mark setup password as used
    conn.execute(
        "UPDATE tenants SET setup_used = 1 WHERE id = ?1",
        params![tenant_id],
    )?;

    // Generate a phone token for this tenant
    let phone_token = generate_random_token();
    let phone_token_hash = hash_token(&phone_token);
    conn.execute(
        "INSERT INTO phone_tokens (tenant_id, token_hash, created_at)
         VALUES (?1, ?2, datetime('now'))",
        params![tenant_id, phone_token_hash],
    )?;

    tracing::info!(tenant_id = %tenant_id, cred_id = %cred_id, "Credential enrolled");

    Ok(tenant_id)
}

/// Mint a new agent token (called by the CLI).
pub fn mint_agent_token(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    scope: &str,
    ttl_secs: u64,
) -> Result<String> {
    let token = format!("stronghold_agent_{}", generate_random_token());
    let token_hash = hash_token(&token);
    let expires_at = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::seconds(ttl_secs as i64))
        .unwrap()
        .to_rfc3339();

    let conn = db.get()?;
    conn.execute(
        "INSERT INTO agent_tokens (tenant_id, token_hash, scope, created_at, expires_at)
         VALUES (?1, ?2, ?3, datetime('now'), ?4)",
        params![tenant_id, token_hash, scope, expires_at],
    )?;

    tracing::info!(tenant_id = %tenant_id, scope = scope, "Agent token minted");

    Ok(token)
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
