//! Audit log writer — append signed entries to the per-tenant SQLite database.
//!
//! Implemented in: W5-T1 (entry writer), W5-T4 (key rotation)
//! Tested by: gateway/src/audit/log.rs (unit tests)

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crypto::hybrid_sig::AuditKeys;

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
    let message = format!(
        "{}|{}|{}|{}|{}|{}",
        ts, tenant_id, machine_id, event, payload, prev_hash
    );

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

/// Rotate the audit signing keys.
///
/// Ceremony (W5-T4):
/// 1. Generate a new `AuditKeys` keypair.
/// 2. Write a `key_rotation` audit entry signed with the OLD keys. The
///    payload records the new Ed25519 public key fingerprint so the
///    rotation is provable from the log alone.
/// 3. All subsequent entries must be signed with the new keys.
///
/// The old keys are NOT deleted — they remain on disk so historical
/// entries can still be verified offline (`audit verify` walks the log
/// and uses the key whose fingerprint matches each entry).
///
/// Returns the new `AuditKeys`. The caller is responsible for persisting
/// them (e.g. via `AuditKeys::save`).
pub fn rotate_audit_keys(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    machine_id: &str,
    old_keys: &AuditKeys,
) -> Result<AuditKeys> {
    let new_keys = AuditKeys::generate();
    let (new_ed_fp, _new_mldsa_fp) = new_keys.fingerprints();
    let (old_ed_fp, _old_mldsa_fp) = old_keys.fingerprints();

    let payload = serde_json::json!({
        "rotation": "audit_keys",
        "old_ed25519_fingerprint": old_ed_fp,
        "new_ed25519_fingerprint": new_ed_fp,
        "reason": "scheduled_rotation",
    });

    // The rotation entry itself is signed with the OLD keys so a verifier
    // walking the chain can authenticate the rotation.
    entry(db, tenant_id, machine_id, "key_rotation", payload, old_keys)?;

    tracing::info!(
        tenant = %tenant_id,
        old_fp = %old_ed_fp,
        new_fp = %new_ed_fp,
        "Audit keys rotated"
    );

    Ok(new_keys)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::hybrid_sig::DualSignature;
    use crate::db::init_memory_pool;
    use rusqlite::params;
    use sha2::Digest;

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

    /// Read all audit rows for a tenant ordered by seq.
    fn fetch_entries(
        pool: &Pool<SqliteConnectionManager>,
        tenant_id: &str,
    ) -> Vec<(
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    )> {
        let conn = pool.get().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT seq, ts, machine_id, event, payload, prev_hash, hash,
                        sig_ed25519, sig_mldsa65
                 FROM audit_entries
                 WHERE tenant_id = ?1
                 ORDER BY seq ASC",
            )
            .unwrap();
        stmt.query_map([tenant_id], |row| {
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
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    #[test]
    fn test_write_single_entry() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_a");
        let keys = AuditKeys::generate();

        entry(
            &pool,
            "tenant_a",
            "machine_1",
            "session_started",
            serde_json::json!({"image": "rocky-base"}),
            &keys,
        )
        .unwrap();

        let rows = fetch_entries(&pool, "tenant_a");
        assert_eq!(rows.len(), 1);
        let (_, ts, machine, event, payload, prev_hash, hash, sig_ed, _sig_ml) = &rows[0];
        assert!(!ts.is_empty());
        assert_eq!(machine, "machine_1");
        assert_eq!(event, "session_started");
        assert!(payload.contains("rocky-base"));
        // First entry's prev_hash must be the zero hash.
        assert_eq!(
            prev_hash,
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
        // Hash is 64 hex chars (SHA-256).
        assert_eq!(hash.len(), 64);
        // Ed25519 sig is non-empty (base64 of 64 bytes).
        assert!(!sig_ed.is_empty());
    }

    #[test]
    fn test_write_100_entries_hash_chain_intact() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_b");
        let keys = AuditKeys::generate();

        for i in 0..100 {
            entry(
                &pool,
                "tenant_b",
                "machine_1",
                "command_executed",
                serde_json::json!({"cmd": format!("echo {}", i)}),
                &keys,
            )
            .unwrap();
        }

        let rows = fetch_entries(&pool, "tenant_b");
        assert_eq!(rows.len(), 100);

        // Walk the chain: each entry's prev_hash must equal the previous entry's hash.
        let mut expected_prev =
            "0000000000000000000000000000000000000000000000000000000000000000".to_string();
        for (seq, ts, machine, event, payload, prev_hash, hash, _sig_ed, _sig_ml) in &rows {
            assert_eq!(
                prev_hash, &expected_prev,
                "seq {}: hash chain broken at prev_hash",
                seq
            );
            // Recompute the hash and verify it matches.
            let message = format!(
                "{}|{}|{}|{}|{}|{}",
                ts, "tenant_b", machine, event, payload, prev_hash
            );
            let mut h = Sha256::new();
            h.update(message.as_bytes());
            let computed = hex::encode(h.finalize());
            assert_eq!(&computed, hash, "seq {}: stored hash mismatch", seq);
            expected_prev = hash.clone();
        }
    }

    #[test]
    fn test_all_signatures_verify() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_c");
        let keys = AuditKeys::generate();

        for i in 0..25 {
            entry(
                &pool,
                "tenant_c",
                "machine_1",
                "command_executed",
                serde_json::json!({"cmd": format!("ls /tmp/{}", i)}),
                &keys,
            )
            .unwrap();
        }

        let rows = fetch_entries(&pool, "tenant_c");
        assert_eq!(rows.len(), 25);

        // Every Ed25519 signature must verify against the recomputed message.
        for (seq, ts, machine, event, payload, prev_hash, _hash, sig_ed, sig_ml) in &rows {
            let message = format!(
                "{}|{}|{}|{}|{}|{}",
                ts, "tenant_c", machine, event, payload, prev_hash
            );
            let sig = DualSignature {
                sig_ed25519: sig_ed.clone(),
                sig_mldsa65: sig_ml.clone(),
            };
            assert!(
                keys.verify(message.as_bytes(), &sig),
                "seq {}: signature verification failed",
                seq
            );
        }
    }

    #[test]
    fn test_tampered_payload_breaks_signature() {
        // Write one entry, then flip a bit in the payload and verify the
        // signature no longer matches.
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_d");
        let keys = AuditKeys::generate();

        entry(
            &pool,
            "tenant_d",
            "machine_1",
            "command_executed",
            serde_json::json!({"cmd": "echo hello"}),
            &keys,
        )
        .unwrap();

        // Tamper: rewrite the payload to something the signature wasn't made over.
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "UPDATE audit_entries SET payload = ?1 WHERE tenant_id = ?2 AND seq = 1",
                params![r#"{"cmd":"rm -rf /"}"#, "tenant_d"],
            )
            .unwrap();
        }

        let rows = fetch_entries(&pool, "tenant_d");
        let (_seq, ts, machine, event, payload, prev_hash, _hash, sig_ed, sig_ml) = &rows[0];

        // Recompute the message over the (tampered) payload.
        let message = format!(
            "{}|{}|{}|{}|{}|{}",
            ts, "tenant_d", machine, event, payload, prev_hash
        );
        let sig = DualSignature {
            sig_ed25519: sig_ed.clone(),
            sig_mldsa65: sig_ml.clone(),
        };
        assert!(
            !keys.verify(message.as_bytes(), &sig),
            "tampered payload must fail signature verification"
        );
    }

    #[test]
    fn test_tampered_hash_breaks_chain() {
        // Write two entries, then mutate the hash of the first one. The
        // second entry's prev_hash no longer matches.
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_e");
        let keys = AuditKeys::generate();

        entry(
            &pool,
            "tenant_e",
            "m",
            "e1",
            serde_json::json!({"n": 1}),
            &keys,
        )
        .unwrap();
        entry(
            &pool,
            "tenant_e",
            "m",
            "e2",
            serde_json::json!({"n": 2}),
            &keys,
        )
        .unwrap();

        // Tamper: rewrite the hash of seq=1 to a bogus value.
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "UPDATE audit_entries SET hash = ?1 WHERE tenant_id = ?2 AND seq = 1",
                params![
                    "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                    "tenant_e"
                ],
            )
            .unwrap();
        }

        let rows = fetch_entries(&pool, "tenant_e");
        let (_, _ts, _m, _e, _p, _prev1, hash1, _sig1, _sigml1) = &rows[0];
        let (_, _ts, _m, _e, _p, prev2, _hash2, _sig2, _sigml2) = &rows[1];

        // seq=2.prev_hash must NOT equal seq=1.hash anymore.
        assert_ne!(prev2, hash1, "tampered hash should break the chain");
    }

    #[test]
    fn test_first_entry_prev_hash_is_zero() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_f");
        let keys = AuditKeys::generate();

        entry(
            &pool,
            "tenant_f",
            "m",
            "boot",
            serde_json::json!({}),
            &keys,
        )
        .unwrap();

        let rows = fetch_entries(&pool, "tenant_f");
        let (_, _ts, _m, _e, _p, prev_hash, _h, _sig, _sigml) = &rows[0];
        assert_eq!(
            prev_hash,
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn test_two_tenants_have_independent_chains() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_g1");
        seed_tenant(&pool, "tenant_g2");
        let keys = AuditKeys::generate();

        entry(
            &pool,
            "tenant_g1",
            "m",
            "a",
            serde_json::json!({}),
            &keys,
        )
        .unwrap();
        entry(
            &pool,
            "tenant_g2",
            "m",
            "b",
            serde_json::json!({}),
            &keys,
        )
        .unwrap();
        entry(
            &pool,
            "tenant_g1",
            "m",
            "c",
            serde_json::json!({}),
            &keys,
        )
        .unwrap();

        let g1 = fetch_entries(&pool, "tenant_g1");
        let g2 = fetch_entries(&pool, "tenant_g2");
        assert_eq!(g1.len(), 2);
        assert_eq!(g2.len(), 1);

        // Both first entries must start from the zero hash.
        assert_eq!(
            g1[0].5,
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            g2[0].5,
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    // --- W5-T4: key rotation ---

    #[test]
    fn test_rotate_audit_keys_returns_new_keypair() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_rot");
        let old_keys = AuditKeys::generate();

        let new_keys = rotate_audit_keys(&pool, "tenant_rot", "machine_1", &old_keys).unwrap();

        // The new keypair must differ from the old one.
        assert_ne!(
            old_keys.ed25519_public_bytes(),
            new_keys.ed25519_public_bytes()
        );
    }

    #[test]
    fn test_rotate_audit_keys_writes_rotation_entry_signed_by_old_keys() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_rot2");
        let old_keys = AuditKeys::generate();

        // Pre-populate with one entry so the rotation isn't the first entry.
        entry(
            &pool,
            "tenant_rot2",
            "m",
            "session_started",
            serde_json::json!({}),
            &old_keys,
        )
        .unwrap();

        let new_keys =
            rotate_audit_keys(&pool, "tenant_rot2", "m", &old_keys).unwrap();

        let rows = fetch_entries(&pool, "tenant_rot2");
        assert_eq!(rows.len(), 2, "expected pre-rotation entry + rotation entry");

        // The rotation entry is seq=2.
        let (seq, _ts, _m, event, payload, _prev, _hash, sig_ed, sig_ml) = &rows[1];
        assert_eq!(*seq, 2);
        assert_eq!(event, "key_rotation");
        assert!(payload.contains("new_ed25519_fingerprint"));

        // The signature must verify with the OLD keys (proving the rotation
        // was authorized by the previous key holder). To recompute the
        // signed message we need the prev_hash of the rotation entry,
        // which is rows[1].5 (already bound to _prev above).
        let prev_hash = &rows[1].5;
        let message = format!(
            "{}|{}|{}|{}|{}|{}",
            _ts, "tenant_rot2", _m, event, payload, prev_hash
        );
        let sig = DualSignature {
            sig_ed25519: sig_ed.clone(),
            sig_mldsa65: sig_ml.clone(),
        };
        assert!(
            old_keys.verify(message.as_bytes(), &sig),
            "rotation entry must verify with old keys"
        );
        // And it must NOT verify with the new keys (it was signed by the old keys).
        assert!(
            !new_keys.verify(message.as_bytes(), &sig),
            "rotation entry must not verify with new keys (it was old-keys-signed)"
        );
    }

    #[test]
    fn test_post_rotation_entries_verify_with_new_keys() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_rot3");
        let old_keys = AuditKeys::generate();

        entry(
            &pool,
            "tenant_rot3",
            "m",
            "before_rotation",
            serde_json::json!({"v": 1}),
            &old_keys,
        )
        .unwrap();
        let new_keys =
            rotate_audit_keys(&pool, "tenant_rot3", "m", &old_keys).unwrap();
        entry(
            &pool,
            "tenant_rot3",
            "m",
            "after_rotation",
            serde_json::json!({"v": 2}),
            &new_keys,
        )
        .unwrap();

        let rows = fetch_entries(&pool, "tenant_rot3");
        assert_eq!(rows.len(), 3);

        // seq=1 (before) verifies with old keys, NOT new keys.
        let (_, ts, m, e, p, ph, _h, sig_ed, sig_ml) = &rows[0];
        let msg = format!("{}|{}|{}|{}|{}|{}", ts, "tenant_rot3", m, e, p, ph);
        let sig = DualSignature {
            sig_ed25519: sig_ed.clone(),
            sig_mldsa65: sig_ml.clone(),
        };
        assert!(old_keys.verify(msg.as_bytes(), &sig));
        assert!(!new_keys.verify(msg.as_bytes(), &sig));

        // seq=3 (after) verifies with new keys, NOT old keys.
        let (_, ts, m, e, p, ph, _h, sig_ed, sig_ml) = &rows[2];
        let msg = format!("{}|{}|{}|{}|{}|{}", ts, "tenant_rot3", m, e, p, ph);
        let sig = DualSignature {
            sig_ed25519: sig_ed.clone(),
            sig_mldsa65: sig_ml.clone(),
        };
        assert!(new_keys.verify(msg.as_bytes(), &sig));
        assert!(!old_keys.verify(msg.as_bytes(), &sig));
    }

    #[test]
    fn test_rotation_preserves_hash_chain() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_rot4");
        let old_keys = AuditKeys::generate();

        entry(
            &pool,
            "tenant_rot4",
            "m",
            "a",
            serde_json::json!({}),
            &old_keys,
        )
        .unwrap();
        let new_keys =
            rotate_audit_keys(&pool, "tenant_rot4", "m", &old_keys).unwrap();
        entry(
            &pool,
            "tenant_rot4",
            "m",
            "b",
            serde_json::json!({}),
            &new_keys,
        )
        .unwrap();

        let rows = fetch_entries(&pool, "tenant_rot4");
        assert_eq!(rows.len(), 3);
        // Chain: prev_hash[1] == hash[0], prev_hash[2] == hash[1].
        assert_eq!(rows[1].5, rows[0].6);
        assert_eq!(rows[2].5, rows[1].6);
    }
}
