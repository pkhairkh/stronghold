//! Vultr VPS escalation — boot dedicated VPS on demand.
//!
//! For workloads needing more than any worker has (GPU, large memory),
//! the gateway calls the Vultr API to boot a fresh Rocky VPS, joins it
//! to the k3s cluster as an ephemeral worker, schedules the pod, and
//! destroys the VPS when the session ends.

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct EscalationRequest {
    pub label: String,
    pub plan: String,   // Vultr plan ID
    pub region: String, // Vultr region ID
    pub image: String,  // OCI image to run
    pub gpu: bool,
    pub ttl_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EscalationResponse {
    pub vps_id: String,
    pub ip: String,
    pub created_at: String,
}

/// Boot a dedicated VPS via the Vultr API.
pub async fn boot_vps(req: &EscalationRequest) -> Result<EscalationResponse> {
    tracing::info!(
        label = %req.label,
        plan = %req.plan,
        region = %req.region,
        gpu = req.gpu,
        "Booting dedicated VPS via Vultr API (stub)"
    );

    // TODO: implement actual Vultr API call
    // POST https://api.vultr.com/v2/instances
    // with cloud-init script that:
    //   1. installs k3s worker
    //   2. joins cluster
    //   3. pulls OCI image
    //   4. runs pod as init process

    Ok(EscalationResponse {
        vps_id: "stub-vps-id".to_string(),
        ip: "0.0.0.0".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Destroy a VPS.
pub async fn destroy_vps(vps_id: &str) -> Result<()> {
    tracing::info!(vps = vps_id, "Destroying VPS (stub)");

    // TODO: implement actual Vultr API call
    // DELETE https://api.vultr.com/v2/instances/{vps_id}

    Ok(())
}

/// Snapshot a VPS's volumes to Vultr object storage before destruction.
pub async fn snapshot_volumes(vps_id: &str) -> Result<String> {
    tracing::info!(vps = vps_id, "Snapshotting volumes (stub)");

    // TODO: implement snapshot logic

    Ok(format!("snapshot-{}", vps_id))
}
