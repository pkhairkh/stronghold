//! Workflow step executor — creates a pod, runs a task via `kube exec`,
//! captures stdout / stderr / exit_code, and tears the pod down.
//!
//! This is the **V1** half of the workflow engine: each call to
//! [`execute_step`] provisions a fresh `wf-*` pod, waits for it to reach
//! `Ready`, runs the step's `task` instruction via `kube exec`
//! (`sh -c "<task>"`), captures the result, kills the pod, and writes two
//! audit entries (`workflow_step_started` + `workflow_step_completed`).
//!
//! The **V2** half — wave-by-wave DAG advancement — lives in
//! [`crate::workflow::engine::advance_dag`], which calls `execute_step`
//! for each ready step.
//!
//! # Pod lifecycle
//! ```text
//! schedule_workflow_pod ──▶ wait_for_pod_ready ──▶ run_pod_exec ──▶ kill_pod
//!        (scheduler)             (this module)        (this module)   (scheduler)
//! ```
//! The pod is always killed (best effort) before `execute_step` returns,
//! even on error — so a failed run never leaks pods.

use crate::machines::scheduler;
use crate::routes::AppState;
use crate::workflow::engine::Step;
use anyhow::{anyhow, Result};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::api::{Api, AttachParams};
use kube::Client as KubeClient;
use serde::Serialize;
use std::time::{Duration, Instant};

/// Default OCI image for workflow steps that don't specify one.
pub const DEFAULT_IMAGE: &str = "localhost:30500/stronghold/rust-stable:latest";

/// How long to wait for a freshly scheduled pod to reach `Ready`.
const POD_READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Poll interval while waiting for pod readiness.
const POD_READY_POLL: Duration = Duration::from_secs(2);
/// Hard wall-clock timeout for the exec'd command (30 min, matching the
/// engine's `STEP_TIMEOUT`).
const EXEC_TIMEOUT_SECS: u64 = 30 * 60;

/// Result of executing a single workflow step.
///
/// Serialized as `{"exit_code":0,"stdout":"...","stderr":"...","duration_ms":1234}`
/// and stored in `workflow_runs.step_results` keyed by step ID.
#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    /// Process exit code. `0` on success; the process's own code on failure;
    /// `-1` if the exit code could not be determined (e.g. exec timed out
    /// or the kubelet didn't report one).
    pub exit_code: i32,
    /// Captured standard output (UTF-8, lossy on invalid bytes).
    pub stdout: String,
    /// Captured standard error (UTF-8, lossy on invalid bytes).
    pub stderr: String,
    /// Wall-clock duration of the entire step (schedule + exec + cleanup),
    /// in milliseconds.
    pub duration_ms: u64,
}

/// Execute a single workflow step end-to-end.
///
/// 1. Schedules a `wf-*` pod via [`scheduler::schedule_workflow_pod`] using
///    the step's image (or [`DEFAULT_IMAGE`] when the step doesn't specify
///    one).
/// 2. Waits for the pod to reach `Ready` (up to [`POD_READY_TIMEOUT`]).
/// 3. Runs `sh -c "<step.task>"` in the pod via `kube exec`.
/// 4. Captures stdout / stderr / exit_code.
/// 5. Kills the pod via [`scheduler::kill_pod`].
/// 6. Writes `workflow_step_started` + `workflow_step_completed` audit
///    entries.
///
/// Returns the captured [`StepResult`]. The pod is always killed (best
/// effort) before returning, even on error — a failed step never leaks a
/// pod.
///
/// # Errors
/// - `Failed to load tenant for run …` — the run_id doesn't exist.
/// - `schedule pod for step …` — k8s API rejected the pod create.
/// - `Pod … did not become Ready in …` — pod stayed pending past the
///   readiness timeout.
/// - `kube exec failed for …` — the kubelet rejected the exec attach.
/// - `exec timed out after …s` — the command exceeded [`EXEC_TIMEOUT_SECS`].
pub async fn execute_step(state: &AppState, run_id: &str, step: &Step) -> Result<StepResult> {
    let tenant_id = load_tenant_id(state, run_id)?;
    let image = step
        .image
        .clone()
        .unwrap_or_else(|| DEFAULT_IMAGE.to_string());

    // Audit: started (machine_id is empty — pod not yet created).
    let _ = crate::audit::log::entry(
        &state.db,
        &tenant_id,
        "",
        "workflow_step_started",
        serde_json::json!({
            "run_id": run_id,
            "step_id": step.id,
            "image": image,
            "task": step.task,
        }),
        &state.audit_keys,
    );

    let started = Instant::now();

    // 1. Schedule pod.
    let machine = scheduler::schedule_workflow_pod(state, &tenant_id, &image)
        .await
        .map_err(|e| anyhow!("schedule pod for step {}: {}", step.id, e))?;
    let pod_id = machine.id;

    // 2-4. Wait for Ready, exec, capture. Cleanup runs regardless of
    //      whether the inner block succeeds.
    let inner = async {
        wait_for_pod_ready(&pod_id, POD_READY_TIMEOUT).await?;
        run_pod_exec(&pod_id, &step.task, EXEC_TIMEOUT_SECS).await
    }
    .await;

    let duration_ms = started.elapsed().as_millis() as u64;

    // 5. Kill pod (best effort — never block return on cleanup failure).
    //    Use `kill_pod_force` (grace_period=0) so the pod is reaped in
    //    seconds rather than the default 30s `terminationGracePeriodSeconds`.
    if let Err(e) = scheduler::kill_pod_force(state, &pod_id).await {
        tracing::warn!(pod = %pod_id, error = %e, "failed to kill workflow pod");
    }

    let (exit_code, stdout, stderr) = match inner {
        Ok(v) => v,
        Err(e) => {
            // Audit: completed (with error). exit_code -1 signals "didn't run".
            let _ = crate::audit::log::entry(
                &state.db,
                &tenant_id,
                &pod_id,
                "workflow_step_completed",
                serde_json::json!({
                    "run_id": run_id,
                    "step_id": step.id,
                    "exit_code": -1,
                    "error": e.to_string(),
                    "duration_ms": duration_ms,
                }),
                &state.audit_keys,
            );
            return Err(e);
        }
    };

    // 6. Audit: completed.
    let _ = crate::audit::log::entry(
        &state.db,
        &tenant_id,
        &pod_id,
        "workflow_step_completed",
        serde_json::json!({
            "run_id": run_id,
            "step_id": step.id,
            "exit_code": exit_code,
            "stdout_len": stdout.len(),
            "stderr_len": stderr.len(),
            "duration_ms": duration_ms,
        }),
        &state.audit_keys,
    );

    Ok(StepResult {
        exit_code,
        stdout,
        stderr,
        duration_ms,
    })
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Look up the tenant_id for a workflow run (for audit attribution + pod
/// labels).
fn load_tenant_id(state: &AppState, run_id: &str) -> Result<String> {
    let conn = state.db.get().map_err(|e| anyhow!("DB pool error: {}", e))?;
    let tenant_id: String = conn
        .query_row(
            "SELECT tenant_id FROM workflow_runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .map_err(|e| anyhow!("Failed to load tenant for run {}: {}", run_id, e))?;
    Ok(tenant_id)
}

/// Get a Kubernetes client connected to the local k3s cluster.
///
/// Mirrors [`crate::machines::scheduler::get_kube_client`]: try in-cluster
/// config first, then fall back to inferring from `KUBECONFIG` or the
/// default k3s kubeconfig path.
async fn get_kube_client() -> Result<KubeClient> {
    if let Ok(config) = kube::Config::incluster() {
        Ok(KubeClient::try_from(config)?)
    } else {
        let config = kube::Config::infer().await?;
        Ok(KubeClient::try_from(config)?)
    }
}

/// Wait for a pod to reach `Ready=True`, polling every [`POD_READY_POLL`].
///
/// Returns `Ok(())` once Ready; `Err` if [`POD_READY_TIMEOUT`] elapses
/// first. A 404 while polling is treated as "not yet visible" and retried
/// — the k8s API may briefly return 404 for a pod that was just created.
async fn wait_for_pod_ready(pod_name: &str, timeout: Duration) -> Result<()> {
    let client = get_kube_client().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            return Err(anyhow!(
                "Pod {} did not become Ready in {:?}",
                pod_name,
                timeout
            ));
        }
        match pods.get(pod_name).await {
            Ok(pod) => {
                if is_pod_ready(&pod) {
                    return Ok(());
                }
            }
            Err(kube::Error::Api(e)) if e.code == 404 => {
                // Pod not yet visible — keep polling.
            }
            Err(e) => {
                tracing::warn!(pod = pod_name, error = %e, "Error polling pod status");
            }
        }
        tokio::time::sleep(POD_READY_POLL).await;
    }
}

/// True if the pod has a `Ready` condition with status `True`.
fn is_pod_ready(pod: &Pod) -> bool {
    let Some(status) = pod.status.as_ref() else {
        return false;
    };
    let Some(conditions) = status.conditions.as_ref() else {
        return false;
    };
    conditions.iter().any(|c| c.type_ == "Ready" && c.status == "True")
}

/// Run `sh -c "<task>"` in the pod via `kube exec` and capture stdout,
/// stderr, and exit code.
///
/// Mirrors the exec pattern in `routes::exec::run_pod_exec` (which is
/// private to that module), but returns `anyhow::Result` instead of an
/// HTTP-typed `Result` since this is called from the workflow engine, not
/// a route handler.
async fn run_pod_exec(
    pod_name: &str,
    task: &str,
    timeout_secs: u64,
) -> Result<(i32, String, String)> {
    use tokio::io::AsyncReadExt;

    let client = get_kube_client().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);

    let ap = AttachParams::default()
        .stdin(false)
        .stdout(true)
        .stderr(true)
        .tty(false);

    let mut exec = pods
        .exec(pod_name, vec!["sh", "-c", task], &ap)
        .await
        .map_err(|e| anyhow!("kube exec failed for {}: {}", pod_name, e))?;

    let status_fut = exec.take_status();
    let stdout_reader = exec.stdout();
    let stderr_reader = exec.stderr();
    let timeout_dur = Duration::from_secs(timeout_secs.max(1));

    let outcome = tokio::select! {
        result = async {
            let (status_result, stdout, stderr) = tokio::join!(
                async {
                    match status_fut {
                        Some(fut) => fut.await,
                        None => None,
                    }
                },
                async {
                    let mut out = String::new();
                    if let Some(mut r) = stdout_reader {
                        let _ = r.read_to_string(&mut out).await;
                    }
                    out
                },
                async {
                    let mut err = String::new();
                    if let Some(mut r) = stderr_reader {
                        let _ = r.read_to_string(&mut err).await;
                    }
                    err
                },
            );
            let exit_code = match status_result {
                Some(status) => parse_exit_code(&status),
                None => 0, // No status object — pipe EOF'd cleanly, assume success.
            };
            (exit_code, stdout, stderr)
        } => Ok(result),
        _ = tokio::time::sleep(timeout_dur) => {
            exec.abort();
            Err(anyhow!("exec timed out after {}s", timeout_secs))
        }
    };

    outcome
}

/// Parse the exit code from a kubelet `Status` response.
///
/// On `Success` → `0`. Otherwise, looks for the last whitespace-separated
/// token in `status.message` (e.g. the "1" in `"command terminated with
/// exit code 1"`) and parses it as an integer. Falls back to `-1` (unknown).
fn parse_exit_code(status: &Status) -> i32 {
    if status.status.as_deref() == Some("Success") {
        return 0;
    }
    if let Some(msg) = &status.message {
        if let Some(token) = msg.split_whitespace().next_back() {
            if let Ok(code) = token.parse::<i32>() {
                return code;
            }
        }
    }
    -1
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Pod, PodCondition, PodStatus};

    // ─── parse_exit_code ─────────────────────────────────────────────────

    #[test]
    fn test_parse_exit_code_success() {
        let s = Status {
            status: Some("Success".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&s), 0);
    }

    #[test]
    fn test_parse_exit_code_nonzero() {
        // containerd-style message: "command terminated with exit code 42"
        let s = Status {
            status: Some("Failure".to_string()),
            message: Some("command terminated with exit code 42".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&s), 42);
    }

    #[test]
    fn test_parse_exit_code_unknown_no_message() {
        let s = Status {
            status: Some("Failure".to_string()),
            message: None,
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&s), -1);
    }

    #[test]
    fn test_parse_exit_code_unknown_unparseable_message() {
        let s = Status {
            status: Some("Failure".to_string()),
            message: Some("some non-numeric reason".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&s), -1);
    }

    // ─── is_pod_ready ────────────────────────────────────────────────────

    fn pod_with_ready(ready: bool) -> Pod {
        Pod {
            status: Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "Ready".to_string(),
                    status: if ready {
                        "True".to_string()
                    } else {
                        "False".to_string()
                    },
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_is_pod_ready_true() {
        assert!(is_pod_ready(&pod_with_ready(true)));
    }

    #[test]
    fn test_is_pod_ready_false() {
        assert!(!is_pod_ready(&pod_with_ready(false)));
    }

    #[test]
    fn test_is_pod_ready_no_status() {
        let pod = Pod {
            status: None,
            ..Default::default()
        };
        assert!(!is_pod_ready(&pod));
    }

    #[test]
    fn test_is_pod_ready_no_conditions() {
        let pod = Pod {
            status: Some(PodStatus {
                conditions: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!is_pod_ready(&pod));
    }

    #[test]
    fn test_is_pod_ready_other_condition_only() {
        // Pod with a ContainersReady condition but no Ready — not ready.
        let pod = Pod {
            status: Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "ContainersReady".to_string(),
                    status: "True".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!is_pod_ready(&pod));
    }

    // ─── StepResult serialization ────────────────────────────────────────

    #[test]
    fn test_step_result_serializes_to_expected_shape() {
        // The shape must match what workflow_runs.step_results stores:
        // {"exit_code":0,"stdout":"hello\n","stderr":"","duration_ms":1234}
        let r = StepResult {
            exit_code: 0,
            stdout: "hello\n".to_string(),
            stderr: String::new(),
            duration_ms: 1234,
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["stdout"], "hello\n");
        assert_eq!(v["stderr"], "");
        assert_eq!(v["duration_ms"], 1234);
    }

    // ─── k3s integration (manual; #[ignore] by default) ─────────────────
    //
    // These tests provision real `wf-*` pods on the dev box's k3s cluster.
    // They are `#[ignore]`'d so they don't run under `cargo test workflow`
    // (which the DoD runs in CI). Run manually with:
    //
    //   KUBECONFIG=/etc/rancher/k3s/k3s.yaml \
    //     cargo test --features no-sev-snp workflow::executor -- --ignored
    //
    // Each test skips gracefully if the kube client can't be created
    // (e.g. when run on a machine without k3s).

    /// Build an AppState backed by an in-memory SQLite DB with one tenant,
    /// one workflow, and one workflow_run row (`r1`) ready for `execute_step`.
    fn integration_state() -> Option<AppState> {
        let pool = crate::db::init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('t_int', 'T', datetime('now'), 'x', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workflows (id, tenant_id, name, dag, status, created_at)
             VALUES ('wf_int', 't_int', 'W', '{\"steps\":[]}', 'active', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workflow_runs
             (id, workflow_id, tenant_id, status, current_steps, completed_steps, started_at)
             VALUES ('r1', 'wf_int', 't_int', 'running', '[]', '[]', datetime('now'))",
            [],
        )
        .unwrap();
        drop(conn);

        // Probe k3s reachability — skip the test if not running on the dev box.
        // We can't `await` here (this is a sync fn), so we just return the
        // state and let the first `execute_step` call fail+skip if k8s is
        // unreachable.
        let keys = crate::crypto::hybrid_sig::AuditKeys::generate();
        let push_keys = crate::crypto::hybrid_kem::PushKeys::generate();
        Some(AppState {
            db: pool,
            audit_keys: keys,
            push_keys,
            pty_registry: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        })
    }

    /// Helper: skip a test gracefully if k8s is unreachable. Returns `true`
    /// if the test should be skipped.
    async fn skip_if_no_k8s() -> bool {
        // Install the rustls CryptoProvider (main.rs does this in prod, but
        // test binaries don't run main.rs). Idempotent via `Once`.
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
        match crate::machines::scheduler::list_pods().await {
            Ok(_) => false,
            Err(e) => {
                eprintln!("skipping k3s integration test: {}", e);
                true
            }
        }
    }

    /// DoD: `execute_step` returns a StepResult for a simple `echo hello` task.
    #[tokio::test]
    #[ignore]
    async fn integration_execute_step_echo_hello() {
        if skip_if_no_k8s().await {
            return;
        }
        let state = integration_state().unwrap();
        let step = Step {
            id: "s1".to_string(),
            task: "echo hello".to_string(),
            image: None, // uses DEFAULT_IMAGE
            ttl_secs: None,
            context: None,
            depends_on: vec![],
            condition: None,
            max_retries: Some(0), // don't retry — fail fast
        };
        let result = execute_step(&state, "r1", &step).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "hello");
        assert!(result.stderr.is_empty());
        assert!(result.duration_ms > 0);

        // Verify no `wf-*` pod leaked. `kill_pod` returns as soon as the
        // deletion is *initiated* — the pod may still be visible for a few
        // seconds while k8s actually terminates it, so poll briefly.
        assert_no_workflow_pods_leaked().await;
    }

    /// DoD: a failing task returns a non-zero exit code.
    #[tokio::test]
    #[ignore]
    async fn integration_execute_step_nonzero_exit() {
        if skip_if_no_k8s().await {
            return;
        }
        let state = integration_state().unwrap();
        // `exit 7` — distinguishable from -1 (unknown) and 1 (generic fail).
        let step = Step {
            id: "s1".to_string(),
            task: "exit 7".to_string(),
            image: None,
            ttl_secs: None,
            context: None,
            depends_on: vec![],
            condition: None,
            max_retries: Some(0),
        };
        let result = execute_step(&state, "r1", &step).await.unwrap();
        assert_eq!(result.exit_code, 7);
        assert_no_workflow_pods_leaked().await;
    }

    /// Poll `list_pods` for up to ~15s waiting for all `wf-*` pods to
    /// disappear. `kill_pod` returns as soon as deletion is initiated, but
    /// the pod may remain visible (in `Terminating` state) for a few
    /// seconds while k8s actually reaps it.
    async fn assert_no_workflow_pods_leaked() {
        for _ in 0..30 {
            let pods = crate::machines::scheduler::list_pods().await.unwrap();
            let leaked: Vec<_> = pods.iter().filter(|p| p.starts_with("wf-")).collect();
            if leaked.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let pods = crate::machines::scheduler::list_pods().await.unwrap();
        let leaked: Vec<_> = pods.iter().filter(|p| p.starts_with("wf-")).collect();
        assert!(leaked.is_empty(), "workflow pods leaked: {:?}", leaked);
    }
}
