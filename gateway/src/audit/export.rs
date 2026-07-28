//! Audit log exporter — export entries in JSON or human-readable format.

use anyhow::Result;
use r2d2::Pool;

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

    let conn = pool.get()?;

    let mut query = String::from(
        "SELECT ts, machine_id, event, payload, hash
         FROM audit_entries
         WHERE tenant_id = ?1"
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
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
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
                    ts, machine, event, &hash[..16], payload
                ));
            }
            Ok(output)
        }
    }
}
