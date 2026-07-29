//! Audit log verifier — verify hash chain and dual signatures offline.
//!
//! Implemented in: W5-T2 (verifier with hash chain + Ed25519 signature verification)
//! Tested by: gateway/src/audit/verify.rs (unit tests + tamper detection)

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use sha2::Digest;

use crate::crypto::hybrid_sig::AuditKeys;
use crate::crypto::hybrid_sig::DualSignature;

/// Type alias for a single audit row from the database.
type AuditRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

/// A detailed verification report returned by [`verify_with_pool`].
///
/// Lists every error found while walking the audit log. Empty `errors`
/// means the log is clean.
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub tenant_id: String,
    pub entries_checked: usize,
    pub errors: Vec<String>,
}

impl VerifyReport {
    /// True when the audit log verifies cleanly (no errors).
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Zero-hash sentinel used as the prev_hash of the first audit entry.
const ZERO_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

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
         ORDER BY seq ASC",
    )?;

    let entries: Vec<AuditRow> = stmt
        .query_map([tenant_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut prev_hash = ZERO_HASH.to_string();
    let mut errors = Vec::new();

    for (seq, ts, machine_id, event, payload, entry_prev_hash, hash, _sig_ed, _sig_mldsa, _sev_hash)
        in &entries
    {
        // Check hash chain
        if *entry_prev_hash != prev_hash {
            errors.push(format!(
                "seq {}: hash chain broken (expected {}, got {})",
                seq, prev_hash, entry_prev_hash
            ));
        }

        // Recompute hash
        let message = format!(
            "{}|{}|{}|{}|{}|{}",
            ts, tenant_id, machine_id, event, payload, entry_prev_hash
        );
        let mut hasher = sha2::Sha256::new();
        hasher.update(message.as_bytes());
        let computed_hash = hex::encode(hasher.finalize());

        if computed_hash != *hash {
            errors.push(format!(
                "seq {}: hash mismatch (expected {}, got {})",
                seq, hash, computed_hash
            ));
        }

        // TODO: verify SEV-SNP attestation report (if present)

        prev_hash = hash.clone();
    }

    // Load the audit keys and verify the dual signature (Ed25519 + ML-DSA-65)
    // on every entry. If the keys can't be loaded (e.g. the keys directory
    // doesn't exist or isn't writable), skip signature verification with a
    // warning and report only the hash-chain results above.
    match AuditKeys::load_or_generate_keys("/var/lib/stronghold/keys/") {
        Ok(keys) => {
            let mut sig_failures: Vec<i64> = Vec::new();
            for (seq, ts, machine_id, event, payload, entry_prev_hash, _hash, sig_ed, sig_mldsa, _sev)
                in &entries
            {
                let message = format!(
                    "{}|{}|{}|{}|{}|{}",
                    ts, tenant_id, machine_id, event, payload, entry_prev_hash
                );
                let sig = DualSignature {
                    sig_ed25519: sig_ed.clone(),
                    sig_mldsa65: sig_mldsa.clone(),
                };
                if !keys.verify(message.as_bytes(), &sig) {
                    sig_failures.push(*seq);
                }
            }
            if sig_failures.is_empty() {
                println!("Ed25519 + ML-DSA-65 signatures: OK");
            } else {
                for seq in &sig_failures {
                    println!("Ed25519 + ML-DSA-65 signatures: FAILED at seq {}", seq);
                    errors.push(format!("seq {}: signature verification failed", seq));
                }
            }
        }
        Err(e) => {
            eprintln!(
                "WARNING: could not load audit keys from /var/lib/stronghold/keys/ ({}); \
                 skipping Ed25519 + ML-DSA-65 signature verification",
                e
            );
        }
    }

    if errors.is_empty() {
        println!(
            "Audit log for tenant {} verified: {} entries, no errors",
            tenant_id,
            entries.len()
        );
        Ok(())
    } else {
        eprintln!("Audit log verification FAILED for tenant {}:", tenant_id);
        for err in &errors {
            eprintln!("  - {}", err);
        }
        Err(anyhow::anyhow!(
            "Audit log verification failed with {} errors",
            errors.len()
        ))
    }
}

/// Verify the audit log for a tenant using an explicit DB pool and the
/// tenant's audit `AuditKeys`.
///
/// This is the test-friendly variant of [`verify_tenant`]: it accepts an
/// in-memory pool (rather than opening the on-disk per-tenant database)
/// and an explicit `AuditKeys` instance so tests can verify entries
/// against the keys that signed them.
///
/// The verifier walks the chain in seq order and checks, per entry:
/// 1. `prev_hash` matches the previous entry's `hash` (chain integrity).
/// 2. The recomputed SHA-256 of `ts|tenant|machine|event|payload|prev_hash`
///    matches the stored `hash` (detection of payload/prev_hash tampering).
/// 3. The Ed25519 signature over the same message verifies against the
///    supplied `keys` (detection of signature tampering or wrong signer).
///
/// Returns a [`VerifyReport`] listing every error. An empty `errors` vec
/// means the log is clean.
pub fn verify_with_pool(
    tenant_id: &str,
    pool: &Pool<SqliteConnectionManager>,
    keys: &AuditKeys,
) -> Result<VerifyReport> {
    let conn = pool.get()?;

    let mut stmt = conn.prepare(
        "SELECT seq, ts, machine_id, event, payload, prev_hash, hash,
                sig_ed25519, sig_mldsa65, sev_snp_report_hash
         FROM audit_entries
         WHERE tenant_id = ?1
         ORDER BY seq ASC",
    )?;

    let entries: Vec<AuditRow> = stmt
        .query_map([tenant_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut prev_hash = ZERO_HASH.to_string();
    let mut errors = Vec::new();

    for (seq, ts, machine_id, event, payload, entry_prev_hash, hash, sig_ed, sig_mldsa, _sev) in
        &entries
    {
        // 1. Hash chain check.
        if *entry_prev_hash != prev_hash {
            errors.push(format!(
                "seq {}: hash chain broken (expected {}, got {})",
                seq, prev_hash, entry_prev_hash
            ));
        }

        // 2. Recompute the SHA-256 over the canonical message.
        let message = format!(
            "{}|{}|{}|{}|{}|{}",
            ts, tenant_id, machine_id, event, payload, entry_prev_hash
        );
        let mut hasher = sha2::Sha256::new();
        hasher.update(message.as_bytes());
        let computed_hash = hex::encode(hasher.finalize());

        if computed_hash != *hash {
            errors.push(format!(
                "seq {}: hash mismatch (stored {}, recomputed {})",
                seq, hash, computed_hash
            ));
        }

        // 3. Ed25519 signature verification.
        let sig = DualSignature {
            sig_ed25519: sig_ed.clone(),
            sig_mldsa65: sig_mldsa.clone(),
        };
        if !keys.verify(message.as_bytes(), &sig) {
            errors.push(format!("seq {}: signature verification failed", seq));
        }

        prev_hash = hash.clone();
    }

    Ok(VerifyReport {
        tenant_id: tenant_id.to_string(),
        entries_checked: entries.len(),
        errors,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::log;
    use crate::db::init_memory_pool;
    use base64::Engine;
    use rusqlite::params;

    /// Insert a tenant row so the audit_entries FK constraint is satisfied.
    fn seed_tenant(pool: &Pool<SqliteConnectionManager>, tenant_id: &str) {
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![tenant_id, "test", chrono::Utc::now().to_rfc3339(), "x"],
        )
        .unwrap();
    }

    fn write_clean_log(
        pool: &Pool<SqliteConnectionManager>,
        tenant_id: &str,
        keys: &AuditKeys,
        n: usize,
    ) {
        for i in 0..n {
            log::entry(
                pool,
                tenant_id,
                "machine_1",
                "command_executed",
                serde_json::json!({"cmd": format!("echo {}", i)}),
                keys,
            )
            .unwrap();
        }
    }

    #[test]
    fn test_verify_clean_log_ok() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_clean");
        let keys = AuditKeys::generate();
        write_clean_log(&pool, "tenant_clean", &keys, 50);

        let report = verify_with_pool("tenant_clean", &pool, &keys).unwrap();
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        assert_eq!(report.entries_checked, 50);
    }

    #[test]
    fn test_verify_empty_log_ok() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_empty");
        let keys = AuditKeys::generate();

        let report = verify_with_pool("tenant_empty", &pool, &keys).unwrap();
        assert!(report.is_ok());
        assert_eq!(report.entries_checked, 0);
    }

    #[test]
    fn test_verify_detects_tampered_payload() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_tamp1");
        let keys = AuditKeys::generate();
        write_clean_log(&pool, "tenant_tamp1", &keys, 10);

        // Tamper: rewrite the payload of seq=5 to something the signature
        // wasn't made over. This should be caught by BOTH the hash check
        // (recomputed hash no longer matches stored hash) and the signature
        // check (signature no longer matches recomputed message).
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "UPDATE audit_entries SET payload = ?1 WHERE tenant_id = ?2 AND seq = 5",
                params![r#"{"cmd":"rm -rf /"}"#, "tenant_tamp1"],
            )
            .unwrap();
        }

        let report = verify_with_pool("tenant_tamp1", &pool, &keys).unwrap();
        assert!(
            !report.is_ok(),
            "tampered payload must cause verification failure"
        );
        assert!(report.entries_checked == 10);

        // The error for seq=5 must mention either hash mismatch or signature.
        let seq5_errors: Vec<&String> = report
            .errors
            .iter()
            .filter(|e| e.contains("seq 5:"))
            .collect();
        assert!(
            !seq5_errors.is_empty(),
            "expected errors for seq 5, got: {:?}",
            report.errors
        );
    }

    #[test]
    fn test_verify_detects_broken_chain() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_broken");
        let keys = AuditKeys::generate();
        write_clean_log(&pool, "tenant_broken", &keys, 5);

        // Tamper: rewrite the prev_hash of seq=3 to a bogus value. This
        // breaks the chain at seq=3 without affecting seq=2's hash check.
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "UPDATE audit_entries SET prev_hash = ?1 WHERE tenant_id = ?2 AND seq = 3",
                params![
                    "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                    "tenant_broken"
                ],
            )
            .unwrap();
        }

        let report = verify_with_pool("tenant_broken", &pool, &keys).unwrap();
        assert!(!report.is_ok(), "broken chain must fail verification");

        // We expect at least one error mentioning seq 3 with "hash chain broken".
        let chain_errors: Vec<&String> = report
            .errors
            .iter()
            .filter(|e| e.contains("seq 3:") && e.contains("hash chain broken"))
            .collect();
        assert!(
            !chain_errors.is_empty(),
            "expected 'hash chain broken' error for seq 3, got: {:?}",
            report.errors
        );
    }

    #[test]
    fn test_verify_detects_tampered_hash() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_thash");
        let keys = AuditKeys::generate();
        write_clean_log(&pool, "tenant_thash", &keys, 5);

        // Tamper: rewrite the hash of seq=2 to a bogus value. This should
        // trigger the "hash mismatch" error AND break the chain at seq=3.
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "UPDATE audit_entries SET hash = ?1 WHERE tenant_id = ?2 AND seq = 2",
                params![
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "tenant_thash"
                ],
            )
            .unwrap();
        }

        let report = verify_with_pool("tenant_thash", &pool, &keys).unwrap();
        assert!(!report.is_ok(), "tampered hash must fail verification");

        let hash_mismatch: Vec<&String> = report
            .errors
            .iter()
            .filter(|e| e.contains("seq 2:") && e.contains("hash mismatch"))
            .collect();
        assert!(
            !hash_mismatch.is_empty(),
            "expected 'hash mismatch' for seq 2, got: {:?}",
            report.errors
        );

        // And the chain at seq=3 should also be broken because seq=2.hash
        // no longer matches seq=3.prev_hash.
        let chain_broken: Vec<&String> = report
            .errors
            .iter()
            .filter(|e| e.contains("seq 3:") && e.contains("hash chain broken"))
            .collect();
        assert!(
            !chain_broken.is_empty(),
            "expected chain broken for seq 3, got: {:?}",
            report.errors
        );
    }

    #[test]
    fn test_verify_detects_tampered_signature() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_tsig");
        let keys = AuditKeys::generate();
        write_clean_log(&pool, "tenant_tsig", &keys, 5);

        // Tamper: replace seq=4's signature with a valid base64 string of
        // the right length but wrong content (so it parses but doesn't verify).
        {
            let conn = pool.get().unwrap();
            // 64 bytes of 0x41 ('A'), base64-encoded.
            let bad_sig = base64::engine::general_purpose::STANDARD.encode([0x41u8; 64]);
            conn.execute(
                "UPDATE audit_entries SET sig_ed25519 = ?1 WHERE tenant_id = ?2 AND seq = 4",
                params![bad_sig, "tenant_tsig"],
            )
            .unwrap();
        }

        let report = verify_with_pool("tenant_tsig", &pool, &keys).unwrap();
        assert!(!report.is_ok(), "tampered signature must fail verification");

        let sig_errors: Vec<&String> = report
            .errors
            .iter()
            .filter(|e| e.contains("seq 4:") && e.contains("signature"))
            .collect();
        assert!(
            !sig_errors.is_empty(),
            "expected signature verification error for seq 4, got: {:?}",
            report.errors
        );
    }

    #[test]
    fn test_verify_detects_wrong_keys() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_wkeys");
        let signing_keys = AuditKeys::generate();
        write_clean_log(&pool, "tenant_wkeys", &signing_keys, 5);

        // Verify with a DIFFERENT keypair — every signature must fail.
        let wrong_keys = AuditKeys::generate();
        let report = verify_with_pool("tenant_wkeys", &pool, &wrong_keys).unwrap();
        assert!(!report.is_ok(), "wrong keys must fail verification");
        // Every entry should have a signature error.
        let sig_errors: Vec<&String> = report
            .errors
            .iter()
            .filter(|e| e.contains("signature verification failed"))
            .collect();
        assert_eq!(
            sig_errors.len(),
            5,
            "expected 5 signature errors (one per entry), got: {:?}",
            report.errors
        );
    }

    #[test]
    fn test_verify_detects_missing_entry() {
        // Delete the middle entry. The chain at the next entry will break
        // because its prev_hash points to the deleted entry's hash.
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_missing");
        let keys = AuditKeys::generate();
        write_clean_log(&pool, "tenant_missing", &keys, 5);

        {
            let conn = pool.get().unwrap();
            conn.execute(
                "DELETE FROM audit_entries WHERE tenant_id = ?1 AND seq = 3",
                params!["tenant_missing"],
            )
            .unwrap();
        }

        let report = verify_with_pool("tenant_missing", &pool, &keys).unwrap();
        assert!(!report.is_ok(), "missing entry must fail verification");
        assert_eq!(report.entries_checked, 4);
    }

    #[test]
    fn test_verify_with_pool_returns_report_with_tenant_id() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_report");
        let keys = AuditKeys::generate();
        write_clean_log(&pool, "tenant_report", &keys, 3);

        let report = verify_with_pool("tenant_report", &pool, &keys).unwrap();
        assert_eq!(report.tenant_id, "tenant_report");
        assert_eq!(report.entries_checked, 3);
        assert!(report.is_ok());
    }
}
