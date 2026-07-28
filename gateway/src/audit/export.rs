//! Audit log exporter — export entries in JSON or human-readable format.
//!
//! Implemented in: W5-T3 (exporter)
//! Tested by: gateway/src/audit/export.rs (unit tests)

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub struct ExportOptions {
    pub tenant_id: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub machine_id: Option<String>,
    pub format: ExportFormat,
}

pub enum ExportFormat {
    Json,
    Text,
}

/// Export audit log entries.
pub fn export(opts: &ExportOptions) -> Result<String> {
    let db_path = format!("/var/lib/stronghold/audit/{}.db", opts.tenant_id);
    let manager = r2d2_sqlite::SqliteConnectionManager::file(&db_path);
    let pool = Pool::builder().build(manager)?;

    export_with_pool(opts, &pool)
}

/// Export audit log entries using an explicit DB pool.
///
/// This is the test-friendly variant of [`export`]: it accepts an
/// in-memory pool so tests don't have to touch the filesystem.
pub fn export_with_pool(
    opts: &ExportOptions,
    pool: &Pool<SqliteConnectionManager>,
) -> Result<String> {
    let conn = pool.get()?;

    let mut query = String::from(
        "SELECT ts, machine_id, event, payload, hash
         FROM audit_entries
         WHERE tenant_id = ?1",
    );
    let mut params: Vec<String> = vec![opts.tenant_id.clone()];

    if let Some(from) = &opts.from {
        query.push_str(" AND ts >= ?2");
        params.push(from.clone());
    }
    if let Some(to) = &opts.to {
        query.push_str(" AND ts <= ?");
        params.push(to.clone());
    }
    if let Some(machine) = &opts.machine_id {
        query.push_str(" AND machine_id = ?");
        params.push(machine.clone());
    }

    query.push_str(" ORDER BY seq ASC");

    let mut stmt = conn.prepare(&query)?;
    let rows: Vec<(String, String, String, String, String)> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    match opts.format {
        ExportFormat::Json => {
            let entries: Vec<serde_json::Value> = rows.iter().map(|(ts, machine, event, payload, hash)| {
                serde_json::json!({
                    "ts": ts,
                    "machine_id": machine,
                    "event": event,
                    "payload": serde_json::from_str::<serde_json::Value>(payload).unwrap_or(serde_json::Value::Null),
                    "hash": hash,
                })
            }).collect();
            Ok(serde_json::to_string_pretty(&entries)?)
        }
        ExportFormat::Text => {
            let mut output = String::new();
            for (ts, machine, event, payload, hash) in rows {
                output.push_str(&format!(
                    "[{}] machine={} event={} hash={}\n  payload={}\n\n",
                    ts,
                    machine,
                    event,
                    &hash[..16],
                    payload
                ));
            }
            Ok(output)
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::log;
    use crate::crypto::hybrid_sig::AuditKeys;
    use crate::db::init_memory_pool;
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

    fn write_log(
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
    fn test_json_export_count_matches() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_json");
        let keys = AuditKeys::generate();
        write_log(&pool, "tenant_json", &keys, 7);

        let opts = ExportOptions {
            tenant_id: "tenant_json".to_string(),
            from: None,
            to: None,
            machine_id: None,
            format: ExportFormat::Json,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.len(), 7);

        // Each entry must have the expected fields.
        for entry in &parsed {
            assert!(entry.get("ts").is_some());
            assert!(entry.get("machine_id").is_some());
            assert!(entry.get("event").is_some());
            assert!(entry.get("payload").is_some());
            assert!(entry.get("hash").is_some());
        }
    }

    #[test]
    fn test_json_export_empty_log() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_empty");
        let opts = ExportOptions {
            tenant_id: "tenant_empty".to_string(),
            from: None,
            to: None,
            machine_id: None,
            format: ExportFormat::Json,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.len(), 0);
    }

    #[test]
    fn test_json_export_payload_round_trips() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_payload");
        let keys = AuditKeys::generate();
        log::entry(
            &pool,
            "tenant_payload",
            "m",
            "command_executed",
            serde_json::json!({"cmd": "echo hello", "user": "alice"}),
            &keys,
        )
        .unwrap();

        let opts = ExportOptions {
            tenant_id: "tenant_payload".to_string(),
            from: None,
            to: None,
            machine_id: None,
            format: ExportFormat::Json,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.len(), 1);
        let payload = parsed[0].get("payload").unwrap();
        assert_eq!(payload.get("cmd").unwrap(), "echo hello");
        assert_eq!(payload.get("user").unwrap(), "alice");
    }

    #[test]
    fn test_text_export_contains_essentials() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_text");
        let keys = AuditKeys::generate();
        log::entry(
            &pool,
            "tenant_text",
            "machine_42",
            "command_executed",
            serde_json::json!({"cmd": "ls /tmp"}),
            &keys,
        )
        .unwrap();

        let opts = ExportOptions {
            tenant_id: "tenant_text".to_string(),
            from: None,
            to: None,
            machine_id: None,
            format: ExportFormat::Text,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        assert!(out.contains("machine=machine_42"));
        assert!(out.contains("event=command_executed"));
        assert!(out.contains("hash="));
        assert!(out.contains("payload="));
        assert!(out.contains("ls /tmp"));
    }

    #[test]
    fn test_text_export_truncates_hash_to_16_chars() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_hash16");
        let keys = AuditKeys::generate();
        log::entry(
            &pool,
            "tenant_hash16",
            "m",
            "e",
            serde_json::json!({}),
            &keys,
        )
        .unwrap();

        let opts = ExportOptions {
            tenant_id: "tenant_hash16".to_string(),
            from: None,
            to: None,
            machine_id: None,
            format: ExportFormat::Text,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        // Find the hash= prefix.
        let hash_idx = out.find("hash=").unwrap();
        let hash_line = &out[hash_idx..];
        // Take the first line.
        let first_line = hash_line.lines().next().unwrap();
        // The hash is the part after "hash=" up to the first whitespace.
        let hash_part = first_line.strip_prefix("hash=").unwrap();
        let hash_value = hash_part.split_whitespace().next().unwrap();
        assert_eq!(hash_value.len(), 16, "text export must truncate hash to 16 chars");
    }

    #[test]
    fn test_date_range_filter_from() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_range1");
        let keys = AuditKeys::generate();
        write_log(&pool, "tenant_range1", &keys, 5);

        // Capture the ts of seq=3 (the middle entry).
        let mid_ts: String = {
            let conn = pool.get().unwrap();
            conn.query_row(
                "SELECT ts FROM audit_entries WHERE tenant_id = ?1 AND seq = 3",
                params!["tenant_range1"],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Export with from=mid_ts. We should get seq=3, seq=4, seq=5 (3 entries).
        // Because ts is RFC3339 with nanoseconds, ">=" mid_ts will include seq=3.
        let opts = ExportOptions {
            tenant_id: "tenant_range1".to_string(),
            from: Some(mid_ts.clone()),
            to: None,
            machine_id: None,
            format: ExportFormat::Json,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed.len(),
            3,
            "from={} should include seq 3,4,5",
            mid_ts
        );
    }

    #[test]
    fn test_date_range_filter_to() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_range2");
        let keys = AuditKeys::generate();
        write_log(&pool, "tenant_range2", &keys, 5);

        // Capture the ts of seq=2.
        let ts2: String = {
            let conn = pool.get().unwrap();
            conn.query_row(
                "SELECT ts FROM audit_entries WHERE tenant_id = ?1 AND seq = 2",
                params!["tenant_range2"],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Export with to=ts2. Should get seq=1, seq=2.
        let opts = ExportOptions {
            tenant_id: "tenant_range2".to_string(),
            from: None,
            to: Some(ts2.clone()),
            machine_id: None,
            format: ExportFormat::Json,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.len(), 2, "to={} should include seq 1,2", ts2);
    }

    #[test]
    fn test_machine_id_filter() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_mach");
        let keys = AuditKeys::generate();
        // Write entries for two different machines.
        for i in 0..5 {
            log::entry(
                &pool,
                "tenant_mach",
                "machine_alpha",
                "command_executed",
                serde_json::json!({"i": i}),
                &keys,
            )
            .unwrap();
        }
        for i in 0..3 {
            log::entry(
                &pool,
                "tenant_mach",
                "machine_beta",
                "command_executed",
                serde_json::json!({"i": i}),
                &keys,
            )
            .unwrap();
        }

        let opts = ExportOptions {
            tenant_id: "tenant_mach".to_string(),
            from: None,
            to: None,
            machine_id: Some("machine_beta".to_string()),
            format: ExportFormat::Json,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.len(), 3, "machine_beta filter");
        for entry in &parsed {
            assert_eq!(entry.get("machine_id").unwrap(), "machine_beta");
        }
    }

    #[test]
    fn test_combined_from_to_filter() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_combo");
        let keys = AuditKeys::generate();
        write_log(&pool, "tenant_combo", &keys, 10);

        let ts3: String = {
            let conn = pool.get().unwrap();
            conn.query_row(
                "SELECT ts FROM audit_entries WHERE tenant_id = ?1 AND seq = 3",
                params!["tenant_combo"],
                |row| row.get(0),
            )
            .unwrap()
        };
        let ts7: String = {
            let conn = pool.get().unwrap();
            conn.query_row(
                "SELECT ts FROM audit_entries WHERE tenant_id = ?1 AND seq = 7",
                params!["tenant_combo"],
                |row| row.get(0),
            )
            .unwrap()
        };

        let opts = ExportOptions {
            tenant_id: "tenant_combo".to_string(),
            from: Some(ts3),
            to: Some(ts7),
            machine_id: None,
            format: ExportFormat::Json,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        // Should include seq 3,4,5,6,7 = 5 entries.
        assert_eq!(parsed.len(), 5, "from..=to range should include seq 3..7");
    }

    #[test]
    fn test_json_export_ordered_by_seq() {
        let pool = init_memory_pool().unwrap();
        seed_tenant(&pool, "tenant_order");
        let keys = AuditKeys::generate();
        // Write entries with distinct payloads so we can identify them.
        for i in 0..5 {
            log::entry(
                &pool,
                "tenant_order",
                "m",
                "command_executed",
                serde_json::json!({"seq_marker": i}),
                &keys,
            )
            .unwrap();
        }

        let opts = ExportOptions {
            tenant_id: "tenant_order".to_string(),
            from: None,
            to: None,
            machine_id: None,
            format: ExportFormat::Json,
        };
        let out = export_with_pool(&opts, &pool).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        // The seq_marker in the payload must be in ascending order.
        for (i, entry) in parsed.iter().enumerate() {
            let marker = entry
                .get("payload")
                .unwrap()
                .get("seq_marker")
                .unwrap()
                .as_i64()
                .unwrap();
            assert_eq!(marker, i as i64, "entry {} out of order", i);
        }
    }
}
