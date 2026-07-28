//! Audit log writer — append signed entries to the per-tenant SQLite database.

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crypto::hybrid_sig::{AuditKeys, DualSignature};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: String,
    pub tenant_id: String,
    pub machine_id: String,
    pub event: String,
    pub payload: serde_json::Value,
    pub prev_hash: String,
    pub sig_ed25519: String,
    pub sig_mldsa65: String,
    pub sev_snp_report_hash: Option<String>,
}

/// Write an audit entry.
pub fn entry(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    machine_id: &str,
    event: &str,
    payload: serde_json::Value,
    keys: &AuditKeys,
) -> Result<()> {
    let conn = db.get()?;
    let ts = chrono::Utc::now().to_rfc3339();

    // Get previous hash
    let prev_hash: String = conn.query_row(
        "SELECT COALESCE(
            (SELECT hash FROM audit_entries
             WHERE tenant_id = ?1
             ORDER BY seq DESC LIMIT 1),
            '0000000000000000000000000000000000000000000000000000000000000000'
        )",
        params![tenant_id],
        |row| row.get(0),
    )?;

    // Build the message to sign
    let message = format!("{}|{}|{}|{}|{}|{}", ts, tenant_id, machine_id, event, payload, prev_hash);

    // Sign
    let sig = keys.sign(message.as_bytes());

    // Compute hash of this entry
    let mut hasher = Sha256::new();
    hasher.update(message.as_bytes());
    let hash = hex::encode(hasher.finalize());

    // Get SEV-SNP report hash (if running in TEE)
    let sev_snp_report_hash = crate::tee::generate_attestation_report()
        .ok()
        .map(|r| r.report_hash);

    // Insert
    conn.execute(
        "INSERT INTO audit_entries
         (tenant_id, ts, machine_id, event, payload, prev_hash, hash,
          sig_ed25519, sig_mldsa65, sev_snp_report_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            tenant_id,
            ts,
            machine_id,
            event,
            payload.to_string(),
            prev_hash,
            hash,
            sig.sig_ed25519,
            sig.sig_mldsa65,
            sev_snp_report_hash,
        ],
    )?;

    tracing::debug!(
        tenant = %tenant_id,
        machine = %machine_id,
        event = event,
        hash = %hash,
        "Audit entry written"
    );

    Ok(())
}
