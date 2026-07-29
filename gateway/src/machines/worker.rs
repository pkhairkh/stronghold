//! Worker management — add, list, monitor k3s workers.

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    pub host: String,
    pub sev_snp: bool,
    pub cpu_available: u32,
    pub memory_gb_available: u32,
}

/// Add a new worker to the fleet.
pub async fn add(host: &str, _token: &str) -> Result<()> {
    tracing::info!(host = host, "Adding worker");

    // TODO: SSH or cloud-init to install k3s worker
    // For now, just log
    Ok(())
}

/// List all workers.
pub async fn list() -> Result<Vec<Worker>> {
    // TODO: query k3s for registered workers
    Ok(vec![])
}

/// Check if a worker is healthy.
pub async fn health_check(host: &str) -> Result<bool> {
    tracing::debug!(host = host, "Health check (stub)");
    Ok(true)
}
