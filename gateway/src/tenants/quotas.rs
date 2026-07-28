//! Per-tenant resource quotas.
//!
//! Quotas are enforced at the scheduler level. When an agent requests
//! a machine, the scheduler checks:
//! 1. Tenant has not exceeded `max_concurrent_machines`
//! 2. Requested CPU/memory does not exceed per-machine caps
//! 3. Total tenant usage does not exceed `total_cpu_budget` / `total_memory_gb_budget`

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct Quota {
    pub tenant_id: String,
    pub max_concurrent_machines: u32,
    pub max_cpu_per_machine: u32,
    pub max_memory_gb_per_machine: u32,
    pub max_disk_gb_per_machine: u32,
    pub total_cpu_budget: u32,
    pub total_memory_gb_budget: u32,
    pub total_disk_gb_budget: u32,
    pub require_sev_snp_workers: bool,
}

/// Set quotas for a tenant.
pub fn set(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    max_machines: u32,
    max_cpu: u32,
    max_mem: u32,
) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "INSERT OR REPLACE INTO quotas
         (tenant_id, max_concurrent_machines, max_cpu_per_machine, max_memory_gb_per_machine,
          max_disk_gb_per_machine, total_cpu_budget, total_memory_gb_budget,
          total_disk_gb_budget, require_sev_snp_workers)
         VALUES (?1, ?2, ?3, ?4, 100, ?3, ?4, 500, 0)",
        params![tenant_id, max_machines, max_cpu, max_mem],
    )?;
    Ok(())
}

/// Get quotas for a tenant.
pub fn get(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
) -> Result<Quota> {
    let conn = db.get()?;
    let q = conn.query_row(
        "SELECT tenant_id, max_concurrent_machines, max_cpu_per_machine,
                max_memory_gb_per_machine, max_disk_gb_per_machine,
                total_cpu_budget, total_memory_gb_budget,
                total_disk_gb_budget, require_sev_snp_workers
         FROM quotas WHERE tenant_id = ?1",
        params![tenant_id],
        |row| {
            Ok(Quota {
                tenant_id: row.get(0)?,
                max_concurrent_machines: row.get(1)?,
                max_cpu_per_machine: row.get(2)?,
                max_memory_gb_per_machine: row.get(3)?,
                max_disk_gb_per_machine: row.get(4)?,
                total_cpu_budget: row.get(5)?,
                total_memory_gb_budget: row.get(6)?,
                total_disk_gb_budget: row.get(7)?,
                require_sev_snp_workers: row.get(8)?,
            })
        },
    )?;
    Ok(q)
}

/// Check if a tenant can schedule a new machine with the given requirements.
pub fn check_capacity(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    requested_cpu: u32,
    requested_mem_gb: u32,
) -> Result<bool> {
    let quota = get(db, tenant_id)?;

    // Check per-machine caps
    if requested_cpu > quota.max_cpu_per_machine {
        return Ok(false);
    }
    if requested_mem_gb > quota.max_memory_gb_per_machine {
        return Ok(false);
    }

    // Check concurrent machine count
    let conn = db.get()?;
    let active_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM machines
         WHERE tenant_id = ?1 AND status = 'active'",
        params![tenant_id],
        |row| row.get(0),
    )?;

    if active_count as u32 >= quota.max_concurrent_machines {
        return Ok(false);
    }

    // Check total budget
    let current_usage: (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(cpu), 0), COALESCE(SUM(memory_gb), 0)
         FROM machines
         WHERE tenant_id = ?1 AND status = 'active'",
        params![tenant_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    if current_usage.0 as u32 + requested_cpu > quota.total_cpu_budget {
        return Ok(false);
    }
    if current_usage.1 as u32 + requested_mem_gb > quota.total_memory_gb_budget {
        return Ok(false);
    }

    Ok(true)
}
