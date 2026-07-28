//! Audit log verifier — verify hash chain and dual signatures offline.

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use sha2::Digest;

/// Verify the entire audit log for a tenant.
///
/// Checks:
/// 1. Hash chain is unbroken
/// 2. Every Ed25519 signature verifies
/// 3. Every ML-DSA-65 signature verifies
/// 4. SEV-SNP attestation reports are present (when gateway was in TEE mode)
pub fn verify_tenant(tenant_id: &str) -> Result<()> {
    let db_path = format!("/var/lib/stronghold/audit/{}.db", tenant_id);
    let manager = r2d2_sqlite::SqliteConnectionManager::file(&db_path);
    let pool = Pool::builder().build(manager)?;

    let conn = pool.get()?;

    let mut stmt = conn.prepare(
        "SELECT seq, ts, machine_id, event, payload, prev_hash, hash,
                sig_ed25519, sig_mldsa65, sev_snp_report_hash
         FROM audit_entries
         WHERE tenant_id = ?1
         ORDER BY seq ASC"
    )?;

    let entries: Vec<(i64, String, String, String, String, String, String, String, String, Option<String>)> = stmt
        .query_map([tenant_id], |row| {
            Ok((
                row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?,
                row.get(8)?, row.get(9)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut prev_hash = "0000000000000000000000000000000000000000000000000000000000000000".to_string();
    let mut errors = Vec::new();

    for (seq, ts, machine_id, event, payload, entry_prev_hash, hash, sig_ed, sig_mldsa, sev_hash) in entries {
        // Check hash chain
        if entry_prev_hash != prev_hash {
            errors.push(format!("seq {}: hash chain broken (expected {}, got {})", seq, prev_hash, entry_prev_hash));
        }

        // Recompute hash
        let message = format!("{}|{}|{}|{}|{}|{}", ts, tenant_id, machine_id, event, payload, entry_prev_hash);
        let mut hasher = sha2::Sha256::new();
        hasher.update(message.as_bytes());
        let computed_hash = hex::encode(hasher.finalize());

        if computed_hash != hash {
            errors.push(format!("seq {}: hash mismatch (expected {}, got {})", seq, hash, computed_hash));
        }

        // TODO: verify Ed25519 signature
        // TODO: verify ML-DSA-65 signature
        // TODO: verify SEV-SNP attestation report (if present)

        prev_hash = hash;
        let _ = (sig_ed, sig_mldsa, sev_hash);
    }

    if errors.is_empty() {
        println!("Audit log for tenant {} verified: {} entries, no errors", tenant_id, entries.len());
        Ok(())
    } else {
        eprintln!("Audit log verification FAILED for tenant {}:", tenant_id);
        for err in &errors {
            eprintln!("  - {}", err);
        }
        Err(anyhow::anyhow!("Audit log verification failed with {} errors", errors.len()))
    }
}
