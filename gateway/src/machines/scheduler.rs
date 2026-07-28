//! k3s scheduler — schedule and manage pods on the worker fleet.

use anyhow::Result;
use crate::routes::agent::OrderRequest;
use crate::routes::AppState;

pub struct ScheduledMachine {
    pub id: String,
    pub worker: String,
    pub sev_snp_attested: bool,
}

/// Schedule a pod on a worker with available capacity.
pub async fn schedule(
    state: &AppState,
    tenant_id: &str,
    req: &OrderRequest,
) -> Result<ScheduledMachine> {
    // Check tenant quota
    let can_schedule = crate::tenants::quotas::check_capacity(
        &state.db,
        tenant_id,
        req.compute.cpu.unwrap_or(4),
        req.compute.memory_gb.unwrap_or(8),
    )?;

    if !can_schedule {
        return Err(anyhow::anyhow!("Tenant quota exceeded"));
    }

    // Find a worker with capacity
    let worker = find_worker(
        &state,
        req.compute.cpu.unwrap_or(4),
        req.compute.memory_gb.unwrap_or(8),
    ).await?;

    // Schedule the pod via k3s API
    let pod_name = format!("agent-{}", ulid::Ulid::new());
    create_pod(&worker, &pod_name, &req.image, req.compute.cpu.unwrap_or(4), req.compute.memory_gb.unwrap_or(8)).await?;

    tracing::info!(
        tenant = %tenant_id,
        pod = %pod_name,
        worker = %worker.host,
        "Pod scheduled"
    );

    Ok(ScheduledMachine {
        id: pod_name,
        worker: worker.host,
        sev_snp_attested: worker.sev_snp,
    })
}

/// Kill a pod.
pub async fn kill_pod(state: &AppState, machine_id: &str) -> Result<()> {
    tracing::info!(machine = %machine_id, "Killing pod");

    // Find the worker hosting this pod
    let conn = state.db.get()?;
    let worker: String = conn.query_row(
        "SELECT worker FROM machines WHERE id = ?1",
        rusqlite::params![machine_id],
        |row| row.get(0),
    ).unwrap_or_else(|_| "unknown".to_string());

    // Delete the pod via k3s API
    // TODO: implement actual k3s API call
    tracing::info!(machine = %machine_id, worker = %worker, "Pod killed");

    Ok(())
}

/// Open a PTY to a running pod.
pub async fn open_pty(machine_id: &str) -> Result<PtyHandle> {
    tracing::info!(machine = %machine_id, "Opening PTY");
    // TODO: implement containerd exec via k3s API
    Ok(PtyHandle::new())
}

pub struct PtyHandle {
    // TODO: wrap actual pty connection
}

impl PtyHandle {
    fn new() -> Self {
        Self {}
    }

    pub async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let _ = data;
        // TODO: write to containerd exec
        Ok(())
    }

    pub async fn read(&mut self) -> Result<Vec<u8>> {
        // TODO: read from containerd exec
        Ok(Vec::new())
    }
}

async fn find_worker(
    _state: &AppState,
    _cpu: u32,
    _memory_gb: u32,
) -> Result<crate::machines::worker::Worker> {
    // TODO: query k3s for available workers
    Ok(crate::machines::worker::Worker {
        host: "vultr-worker-1".to_string(),
        sev_snp: true,
        cpu_available: 8,
        memory_gb_available: 16,
    })
}

async fn create_pod(
    worker: &crate::machines::worker::Worker,
    pod_name: &str,
    image: &str,
    cpu: u32,
    memory_gb: u32,
) -> Result<()> {
    tracing::info!(
        worker = %worker.host,
        pod = %pod_name,
        image = %image,
        cpu = cpu,
        mem = memory_gb,
        "Creating pod (stub)"
    );
    // TODO: implement k3s API call to create pod
    Ok(())
}
