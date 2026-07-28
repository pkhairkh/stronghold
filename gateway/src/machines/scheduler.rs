//! k3s scheduler — schedule and manage pods on the worker fleet.
//!
//! Implemented in: W3-T9, W3-T10, W3-T11, W3-T14, W3-T15
//! Uses kube-rs to communicate with the k3s API server.

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, Meta};
use kube::Client as KubeClient;
use std::env;

use crate::routes::agent::OrderRequest;
use crate::routes::AppState;

pub struct ScheduledMachine {
    pub id: String,
    pub worker: String,
    pub sev_snp_attested: bool,
}

/// Get a Kubernetes client connected to the local k3s cluster.
async fn get_kube_client() -> Result<KubeClient> {
    // Try in-cluster config first, then fall back to kubeconfig file.
    if let Ok(config) = kube::Config::incluster() {
        Ok(KubeClient::try_from(config)?)
    } else {
        // Use the KUBECONFIG env var or the default k3s path.
        let kubeconfig_path =
            env::var("KUBECONFIG").unwrap_or_else(|_| "/etc/rancher/k3s/k3s.yaml".to_string());
        let config = kube::Config::infer()
            .await
            .with_context(|| format!("inferring kubeconfig (looked for {})", kubeconfig_path))?;
        Ok(KubeClient::try_from(config)?)
    }
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

    // Generate pod name
    let pod_name = format!("agent-{}", ulid::Ulid::new());

    // Get k8s client
    let client = get_kube_client().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);

    // Build pod spec
    let cpu_req = format!("{}m", req.compute.cpu.unwrap_or(4) * 1000);
    let mem_req = format!("{}Gi", req.compute.memory_gb.unwrap_or(8));

    let pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": pod_name,
            "labels": {
                "app": "stronghold-agent",
                "tenant": tenant_id,
                "machine-id": &pod_name,
            }
        },
        "spec": {
            "containers": [{
                "name": "workspace",
                "image": &req.image,
                "command": ["sleep", "infinity"],
                "resources": {
                    "limits": {
                        "cpu": &cpu_req,
                        "memory": &mem_req
                    },
                    "requests": {
                        "cpu": &cpu_req,
                        "memory": &mem_req
                    }
                },
                "volumeMounts": [
                    {"name": "work", "mountPath": "/home/dev/work"},
                    {"name": "cache", "mountPath": "/home/dev/.cache"}
                ]
            }],
            "volumes": [
                {"name": "work", "emptyDir": {}},
                {"name": "cache", "emptyDir": {}}
            ],
            "restartPolicy": "Never"
        }
    }))?;

    // Create the pod
    pods.create(&Default::default(), &pod)
        .await
        .context("failed to create pod")?;

    tracing::info!(
        tenant = tenant_id,
        pod = %pod_name,
        image = %req.image,
        "Pod scheduled"
    );

    Ok(ScheduledMachine {
        id: pod_name,
        worker: "k3s-default".to_string(),
        sev_snp_attested: false,
    })
}

/// Kill a pod.
pub async fn kill_pod(_state: &AppState, machine_id: &str) -> Result<()> {
    tracing::info!(machine = machine_id, "Killing pod");

    let client = get_kube_client().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);

    match pods.delete(machine_id, &DeleteParams::default()).await {
        Ok(_) => {
            tracing::info!(machine = machine_id, "Pod deleted");
            Ok(())
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            tracing::warn!(machine = machine_id, "Pod not found (already deleted?)");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Open a PTY to a running pod.
/// TODO W4-T4: implement via kube exec API (WebSocket).
pub async fn open_pty(machine_id: &str) -> Result<PtyHandle> {
    tracing::info!(machine = machine_id, "Opening PTY (stub — W4-T4 will implement via kube exec)");
    Ok(PtyHandle::new())
}

pub struct PtyHandle {
    // TODO W4-T4: wrap kube exec WebSocket connection
    buffer: Vec<u8>,
}

impl PtyHandle {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    pub async fn read(&mut self) -> Result<Vec<u8>> {
        let data = std::mem::take(&mut self.buffer);
        Ok(data)
    }
}

/// List all running pods (for debugging/monitoring).
pub async fn list_pods() -> Result<Vec<String>> {
    let client = get_kube_client().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);
    let pod_list = pods.list(&ListParams::default()).await?;
    Ok(pod_list.iter().filter_map(|p| Meta::name(p).to_string().into()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_kube_client() {
        // This test requires k3s to be running on the dev box.
        // It will be skipped in CI without k3s.
        let result = get_kube_client().await;
        if result.is_err() {
            eprintln!("Skipping k8s test: k3s not available");
            return;
        }
        assert!(result.is_ok());
    }
}
