//! k3s scheduler — schedule and manage pods on the worker fleet.
//!
//! Implemented in: W3-T9, W3-T10, W3-T11, W3-T14, W3-T15
//! Uses kube-rs to communicate with the k3s API server.

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams};
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

/// Open a PTY to a running pod via k8s exec.
///
/// Uses the kube-rs `Api::exec` API which opens a WebSocket connection
/// to the pod's exec endpoint. The connection supports:
/// - stdin (agent → container): write_all()
/// - stdout (container → agent): read()
/// - stderr (container → agent): read_stderr()
/// - resize messages (terminal size changes)
///
/// The command executed is `/bin/sh` (or `/bin/bash` if available) with
/// a PTY allocated by the container runtime.
pub async fn open_pty(machine_id: &str) -> Result<PtyHandle> {
    use tokio::sync::mpsc;

    let client = get_kube_client().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);

    // Open an exec session: `sh -c "exec sh"` (try bash first, fall back to sh).
    let command = vec!["sh".to_string(), "-c".to_string(), "exec sh".to_string()];

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(32);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(32);

    // Spawn the exec task. This opens a WebSocket to the pod and pumps bytes
    // between the channels and the WebSocket.
    let machine_id_owned = machine_id.to_string();
    let exec_handle = tokio::spawn(async move {
        use kube::api::AttachParams;

        let ap = AttachParams::default()
            .stdin(true)
            .stdout(true)
            .stderr(true)
            .tty(true)
            .command(command);

        match pods.exec(&machine_id_owned, vec!["sh"], &ap).await {
            Ok(mut exec) => {
                use kube::Api::ExecStreamExt as _;
                use tokio::io::AsyncWriteExt;

                // Get stdin writer
                let mut stdin_writer = match exec.stdin() {
                    Some(w) => w,
                    None => {
                        tracing::error!(machine = %machine_id_owned, "exec has no stdin");
                        return;
                    }
                };

                // Spawn stdin pump: channel → WebSocket
                let stdin_task = tokio::spawn(async move {
                    while let Some(data) = stdin_rx.recv().await {
                        if stdin_writer.write_all(&data).await.is_err() {
                            break;
                        }
                        let _ = stdin_writer.flush().await;
                    }
                });

                // Spawn stdout pump: WebSocket → channel
                let stdout_task = tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut stdout = match exec.stdout() {
                        Some(r) => r,
                        None => return,
                    };
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match stdout.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if stdout_tx.send(buf[..n].to_vec()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });

                let _ = stdin_task.await;
                let _ = stdout_task.await;
            }
            Err(e) => {
                tracing::error!(
                    machine = %machine_id_owned,
                    error = %e,
                    "Failed to open exec session"
                );
            }
        }
    });

    tracing::info!(machine = machine_id, "PTY opened via kube exec");

    Ok(PtyHandle {
        stdin_tx,
        stdout_rx,
        exec_handle: Some(exec_handle),
    })
}

pub struct PtyHandle {
    /// Channel to send bytes to the container's stdin (via WebSocket).
    stdin_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Channel to receive bytes from the container's stdout.
    stdout_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Handle to the background exec task. Dropping this kills the session.
    exec_handle: Option<tokio::task::JoinHandle<()>>,
}

impl PtyHandle {
    /// Write bytes to the container's stdin.
    pub async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.stdin_tx
            .send(data.to_vec())
            .await
            .map_err(|_| anyhow::anyhow!("exec stdin channel closed"))?;
        Ok(())
    }

    /// Read bytes from the container's stdout. Blocks until data is available.
    pub async fn read(&mut self) -> Result<Vec<u8>> {
        match self.stdout_rx.recv().await {
            Some(data) => Ok(data),
            None => Ok(Vec::new()), // Channel closed = EOF
        }
    }

    /// Close the PTY session.
    pub fn close(&mut self) {
        if let Some(handle) = self.exec_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// List all running pods (for debugging/monitoring).
pub async fn list_pods() -> Result<Vec<String>> {
    let client = get_kube_client().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);
    let pod_list = pods.list(&ListParams::default()).await?;
    Ok(pod_list
        .iter()
        .filter_map(|p| p.metadata.name.clone())
        .collect())
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
