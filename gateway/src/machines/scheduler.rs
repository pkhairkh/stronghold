//! k3s scheduler — schedule and manage pods on the worker fleet.
//!
//! Implemented in: W3-T9, W3-T10, W3-T11, W3-T14, W3-T15
//! Uses kube-rs to communicate with the k3s API server.

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
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

/// Build the shared workspace PVC name for a tenant.
///
/// All agents belonging to the same tenant share a single PVC named
/// `work-{tenant_id}`, mounted at `/home/dev/work`. Different tenants get
/// different PVCs, providing workspace isolation between tenants. The
/// tenant identifier is sanitized to RFC 1123 subdomain rules (lowercase
/// alphanumeric and `-`) so arbitrary tenant identifiers remain valid
/// Kubernetes object names.
fn pvc_name_for_tenant(tenant_id: &str) -> String {
    let sanitized: String = tenant_id
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    // Fall back to a stable name if sanitization left nothing (e.g. tenant_id
    // was "---"); avoids producing an invalid "work-" name.
    let name = if trimmed.is_empty() { "tenant" } else { trimmed };
    format!("work-{}", name)
}

/// Ensure the shared workspace PVC for a tenant exists, creating it if not.
///
/// Checks for `pvc_name` in the default namespace. If absent, creates a 10Gi
/// PVC bound to the `local-path` storage class (k3s default; override with the
/// `WORK_STORAGE_CLASS` env var) with `ReadWriteOnce` access.
///
/// **Note:** `local-path` only supports `ReadWriteOnce`, so concurrent pods
/// for the same tenant must be co-located on a single node. For true
/// multi-node RWX (read-write-many) collaboration, deploy an NFS or Longhorn
/// storage class and set `WORK_STORAGE_CLASS` accordingly.
async fn ensure_pvc(client: &KubeClient, pvc_name: &str) -> Result<()> {
    let pvcs: Api<PersistentVolumeClaim> = Api::default_namespaced(client.clone());

    // Fast path: PVC already exists for this tenant.
    match pvcs.get(pvc_name).await {
        Ok(_) => {
            tracing::debug!(pvc = pvc_name, "Shared workspace PVC already exists");
            return Ok(());
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Not found — create it below.
        }
        Err(e) => {
            return Err(e).with_context(|| format!("checking workspace PVC {}", pvc_name));
        }
    }

    let storage_class =
        env::var("WORK_STORAGE_CLASS").unwrap_or_else(|_| "local-path".to_string());

    let pvc: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": pvc_name,
            "labels": {
                "app": "stronghold-agent",
                "stronghold.dev/shared-workspace": "true"
            }
        },
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": storage_class,
            "resources": {
                "requests": {
                    "storage": "10Gi"
                }
            }
        }
    }))?;

    pvcs.create(&Default::default(), &pvc)
        .await
        .with_context(|| format!("creating workspace PVC {}", pvc_name))?;

    tracing::info!(
        pvc = pvc_name,
        storage_class = %storage_class,
        "Created shared workspace PVC"
    );

    Ok(())
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

    // Generate pod name — must be a lowercase RFC 1123 subdomain.
    // ULIDs are uppercase by default; lowercase the whole thing. The ULID
    // alphabet (0-9A-HJKMNP-TV-Z) lowercased stays within RFC 1123 charset.
    let pod_name = format!("agent-{}", ulid::Ulid::new().to_string().to_lowercase());

    // Get k8s client
    let client = get_kube_client().await?;

    // Ensure the shared workspace PVC exists for this tenant. All agents for
    // the same tenant mount this PVC at /home/dev/work, enabling multi-agent
    // collaboration. Different tenants get different PVCs (isolation).
    let work_pvc = pvc_name_for_tenant(tenant_id);
    ensure_pvc(&client, &work_pvc).await?;

    let pods: Api<Pod> = Api::default_namespaced(client);

    // Build pod spec
    let cpu_req = format!("{}m", req.compute.cpu.unwrap_or(4) * 1000);
    let mem_req = format!("{}Gi", req.compute.memory_gb.unwrap_or(8));

    // Fetch tenant credentials and inject as env vars
    let env_vars = load_credential_env_vars(&state.db, tenant_id, &state.audit_keys);

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
                "env": env_vars,
                "volumeMounts": [
                    {"name": "work", "mountPath": "/home/dev/work"},
                    {"name": "cache", "mountPath": "/home/dev/.cache"}
                ]
            }],
            "volumes": [
                {"name": "work", "persistentVolumeClaim": {"claimName": &work_pvc}},
                {"name": "cache", "emptyDir": {}}
            ],
            "restartPolicy": "Never",
            "securityContext": {
                "runAsUser": 1000,
                "runAsGroup": 1000,
                "fsGroup": 1000,
                "fsGroupChangePolicy": "OnRootMismatch"
            }
        }
    }))?;

    // Create the pod
    if let Err(e) = pods.create(&Default::default(), &pod).await {
        // Log the full error (including k8s API reason) before propagating
        let err_str = format!("{e:#}");
        tracing::error!(
            pod = %pod_name,
            tenant = tenant_id,
            image = %req.image,
            error = %err_str,
            "k8s pod create failed"
        );
        return Err(e).context("failed to create pod");
    }

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

    // Open an exec session: `sh` for interactive shell.
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
            .tty(true);

        match pods.exec(&machine_id_owned, vec!["sh"], &ap).await {
            Ok(mut exec) => {
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

/// Load tenant credentials that have `env_var` set and return them as
/// Kubernetes env var entries for pod injection.
///
/// Credentials are decrypted using the per-tenant key (derived from the
/// audit Ed25519 key via HKDF). Only credentials with a non-null `env_var`
/// are injected — file-mounted credentials (with `mount_path`) are skipped
/// (they require a Secret + volume mount, which is a TODO).
fn load_credential_env_vars(
    db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    tenant_id: &str,
    audit_keys: &crate::crypto::hybrid_sig::AuditKeys,
) -> Vec<serde_json::Value> {
    let conn = match db.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "DB pool error loading credentials for pod injection");
            return Vec::new();
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT name, encrypted_value, nonce, env_var FROM agent_credentials
         WHERE tenant_id = ?1 AND env_var IS NOT NULL",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Could not query agent_credentials (table may not exist yet)");
            return Vec::new();
        }
    };

    let rows = match stmt.query_map(rusqlite::params![tenant_id], |row| {
        Ok((
            row.get::<_, String>(0)?,    // name
            row.get::<_, Vec<u8>>(1)?,   // encrypted_value
            row.get::<_, Vec<u8>>(2)?,   // nonce
            row.get::<_, String>(3)?,    // env_var
        ))
    }) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Could not fetch credential rows");
            return Vec::new();
        }
    };

    let tenant_key = crate::crypto::vault::derive_tenant_key(tenant_id, audit_keys);
    let mut env_vars = Vec::new();

    for row in rows {
        let (name, encrypted, nonce, env_var) = match row {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "Error reading credential row");
                continue;
            }
        };

        let plaintext = match crate::crypto::vault::decrypt(&encrypted, &nonce, &tenant_key) {
            Ok(p) => String::from_utf8_lossy(&p).to_string(),
            Err(e) => {
                tracing::warn!(error = %e, credential = %name, "Failed to decrypt credential for env injection");
                continue;
            }
        };

        tracing::debug!(
            credential = %name,
            env_var = %env_var,
            "Injecting credential as env var"
        );

        env_vars.push(serde_json::json!({
            "name": env_var,
            "value": plaintext,
        }));
    }

    if !env_vars.is_empty() {
        tracing::info!(
            tenant = tenant_id,
            count = env_vars.len(),
            "Injected {} credential env vars into pod spec",
            env_vars.len()
        );
    }

    env_vars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_kube_client() {
        let result = get_kube_client().await;
        if result.is_err() {
            eprintln!("Skipping k8s test: k3s not available");
            return;
        }
        assert!(result.is_ok());
    }

    #[test]
    fn test_load_credential_env_vars_empty_when_no_credentials() {
        let pool = crate::db::init_memory_pool().unwrap();
        let keys = crate::crypto::hybrid_sig::AuditKeys::generate();
        let env_vars = load_credential_env_vars(&pool, "tenant_test", &keys);
        assert!(env_vars.is_empty());
    }

    #[test]
    fn test_load_credential_env_vars_injects_stored_credentials() {
        let pool = crate::db::init_memory_pool().unwrap();
        let keys = crate::crypto::hybrid_sig::AuditKeys::generate();
        let tenant_key = crate::crypto::vault::derive_tenant_key("tenant_test", &keys);

        // Create the tenant first (FK constraint)
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('tenant_test', 'test', datetime('now'), 'dummy', 1)",
            [],
        ).unwrap();

        // Store a credential
        let (ciphertext, nonce) = crate::crypto::vault::encrypt(b"ghp_secret_token", &tenant_key).unwrap();
        conn.execute(
            "INSERT INTO agent_credentials (id, tenant_id, name, kind, encrypted_value, nonce, env_var, created_at)
             VALUES ('cred_1', 'tenant_test', 'github-pat', 'api_token', ?1, ?2, 'GITHUB_TOKEN', datetime('now'))",
            rusqlite::params![ciphertext, nonce],
        ).unwrap();
        drop(conn);

        let env_vars = load_credential_env_vars(&pool, "tenant_test", &keys);
        assert_eq!(env_vars.len(), 1);
        assert_eq!(env_vars[0]["name"], "GITHUB_TOKEN");
        assert_eq!(env_vars[0]["value"], "ghp_secret_token");
    }

    #[test]
    fn test_pvc_name_for_tenant_basic() {
        assert_eq!(pvc_name_for_tenant("acme"), "work-acme");
    }

    #[test]
    fn test_pvc_name_for_tenant_sanitizes_invalid_chars() {
        // Uppercase, underscores, and punctuation are normalized to RFC 1123.
        assert_eq!(pvc_name_for_tenant("Acme_Corp!"), "work-acme-corp");
    }

    #[test]
    fn test_pvc_name_for_tenant_isolates_tenants() {
        // Different tenants must map to different PVC names so workspaces
        // are isolated between tenants.
        assert_ne!(
            pvc_name_for_tenant("tenant-a"),
            pvc_name_for_tenant("tenant-b")
        );
    }

    #[test]
    fn test_pvc_name_for_tenant_empty_after_sanitize() {
        // A tenant id that sanitizes to nothing must still produce a valid
        // (non-empty) Kubernetes object name.
        assert_eq!(pvc_name_for_tenant("---"), "work-tenant");
    }
}
