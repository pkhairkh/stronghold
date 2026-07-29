//! Worker management — add, list, monitor k3s workers.
//!
//! Workers are k3s agent nodes that join the Stronghold control plane.
//! Each worker box is bootstrapped externally via `setup/worker-bootstrap.sh`
//! (which installs the k3s agent and joins the cluster using the node
//! token). Once registered, workers are discoverable here through the
//! Kubernetes Node API.
//!
//! Implemented in: B6.

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::Client as KubeClient;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    pub host: String,
    pub sev_snp: bool,
    pub cpu_available: u32,
    pub memory_gb_available: u32,
}

/// Get a Kubernetes client connected to the local k3s cluster.
///
/// Tries an in-cluster config first (when the gateway runs as a pod),
/// then falls back to inferring the kubeconfig from `KUBECONFIG` or the
/// default k3s path (`/etc/rancher/k3s/k3s.yaml`).
///
/// This duplicates the helper in `scheduler.rs` to keep this file's
/// scope self-contained (see task B6 constraints).
async fn get_kube_client() -> Result<KubeClient> {
    if let Ok(config) = kube::Config::incluster() {
        Ok(KubeClient::try_from(config)?)
    } else {
        let config = kube::Config::infer()
            .await
            .context("inferring kubeconfig (set KUBECONFIG or run in-cluster)")?;
        Ok(KubeClient::try_from(config)?)
    }
}

/// Add a new worker to the fleet.
///
/// **Workers are not added programmatically by the gateway.** A new
/// worker box must be bootstrapped externally using
/// [`setup/worker-bootstrap.sh`](../../../../setup/worker-bootstrap.sh),
/// which:
///
///   1. Installs the k3s agent.
///   2. Joins it to the control plane using the node token.
///   3. Tags it with capacity / SEV-SNP labels.
///
/// Once the k3s agent registers with the control plane, the worker
/// automatically appears in [`list()`] and becomes schedulable.
///
/// This function therefore only logs the request — the caller (an admin
/// via the CLI) is expected to have already run the bootstrap script on
/// the target box. The `_token` argument is accepted for API
/// compatibility with the CLI's `worker add` command but is unused here.
pub async fn add(host: &str, _token: &str) -> Result<()> {
    tracing::info!(
        host = host,
        "Worker add requested — bootstrap the box externally via setup/worker-bootstrap.sh"
    );
    tracing::info!(
        host = host,
        "Once the k3s agent joins, the worker will appear in `worker::list()`"
    );
    Ok(())
}

/// List all workers (k3s nodes) in the cluster.
///
/// Queries `kube::Api::<Node>` (cluster-scoped) and maps each node to a
/// [`Worker`]:
///
///   - `host`: the node name (typically the hostname).
///   - `cpu_available`: parsed from `status.capacity["cpu"]` — a bare
///     integer is cores; a value suffixed with `m` is milli-cores.
///   - `memory_gb_available`: parsed from `status.capacity["memory"]`
///     (Ki/Mi/Gi/… suffixes converted to whole GB, floored).
///   - `sev_snp`: `true` if the node carries a `sev-snp`/`sev_snp` label
///     set to `"true"`. No workers carry this label today, so the field
///     is `false` unless explicitly tagged.
///
/// Returns an empty `Vec` (not an error) if the cluster has no nodes.
pub async fn list() -> Result<Vec<Worker>> {
    let client = get_kube_client().await?;
    let nodes: Api<Node> = Api::all(client);

    let node_list = nodes.list(&ListParams::default()).await?;

    let mut workers = Vec::with_capacity(node_list.items.len());
    for node in node_list.items {
        let host = node
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "<unknown>".to_string());

        let (cpu_available, memory_gb_available) = parse_capacity(&node);
        let sev_snp = has_sev_snp_label(&node);

        tracing::debug!(
            host = %host,
            cpu_available,
            memory_gb_available,
            sev_snp,
            "Discovered worker node"
        );

        workers.push(Worker {
            host,
            sev_snp,
            cpu_available,
            memory_gb_available,
        });
    }

    tracing::info!(count = workers.len(), "Listed k3s worker nodes");
    Ok(workers)
}

/// Check if a worker (node) is healthy.
///
/// Fetches the named node from the k3s API and inspects its
/// `conditions`. Returns `true` only if a `Ready` condition exists with
/// status `"True"`.
///
/// Returns `false` (not an error) when:
///   - The node does not exist (HTTP 404).
///
/// Returns an error only on transient API failures (auth, network, etc.).
pub async fn health_check(host: &str) -> Result<bool> {
    let client = get_kube_client().await?;
    let nodes: Api<Node> = Api::all(client);

    let node = match nodes.get(host).await {
        Ok(n) => n,
        Err(kube::Error::Api(e)) if e.code == 404 => {
            tracing::warn!(host = host, "Health check: node not found");
            return Ok(false);
        }
        Err(e) => {
            tracing::error!(host = host, error = %e, "Health check: k8s API error");
            return Err(e.into());
        }
    };

    let ready = is_node_ready(&node);
    tracing::debug!(host = host, ready, "Health check");
    Ok(ready)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Extract CPU (cores) and memory (GB) from a node's `status.capacity`.
fn parse_capacity(node: &Node) -> (u32, u32) {
    let capacity = match node.status.as_ref().and_then(|s| s.capacity.as_ref()) {
        Some(c) => c,
        None => return (0, 0),
    };

    let cpu = capacity
        .get("cpu")
        .map(|q| parse_cpu_quantity(&q.0))
        .unwrap_or(0);

    let mem_gb = capacity
        .get("memory")
        .map(|q| parse_memory_quantity(&q.0))
        .unwrap_or(0);

    (cpu, mem_gb)
}

/// Parse a Kubernetes CPU quantity string into a whole-core count.
///
/// Examples:
///   - `"4"`     → 4
///   - `"4000m"` → 4   (milli-cores, divided by 1000, floored)
///   - `"0.5"`   → 0   (fractional cores floored — unusual for capacity)
fn parse_cpu_quantity(s: &str) -> u32 {
    let s = s.trim();
    if let Some(milli) = s.strip_suffix('m') {
        return milli.parse::<u32>().unwrap_or(0) / 1000;
    }
    if let Ok(n) = s.parse::<u32>() {
        return n;
    }
    // Fall back to fractional cores (e.g. "0.5") — floored.
    s.parse::<f64>().map(|f| f as u32).unwrap_or(0)
}

/// Parse a Kubernetes memory quantity string into whole gigabytes.
///
/// Recognizes binary suffixes (`Ki`, `Mi`, `Gi`, `Ti`, `Pi`, `Ei`) and
/// decimal suffixes (`K`, `M`, `G`, `T`, `P`, `E`). A bare number is
/// treated as bytes. The result is floored to the nearest GB.
///
/// Examples:
///   - `"8Gi"`        → 8
///   - `"8153076Ki"`  → 7   (≈7.77 GiB)
///   - `"16384Mi"`    → 16
fn parse_memory_quantity(s: &str) -> u32 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }

    // Split numeric prefix from suffix at the first alphabetic char.
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num_str, suffix) = s.split_at(split);
    let num: f64 = match num_str.parse() {
        Ok(n) => n,
        Err(_) => return 0,
    };

    let bytes: f64 = match suffix {
        "Ki" => num * 1024.0,
        "Mi" => num * 1024.0_f64.powi(2),
        "Gi" => num * 1024.0_f64.powi(3),
        "Ti" => num * 1024.0_f64.powi(4),
        "Pi" => num * 1024.0_f64.powi(5),
        "Ei" => num * 1024.0_f64.powi(6),
        "K" => num * 1e3,
        "M" => num * 1e6,
        "G" => num * 1e9,
        "T" => num * 1e12,
        "P" => num * 1e15,
        "E" => num * 1e18,
        "" => num, // bare bytes
        _ => 0.0,   // unknown suffix
    };

    (bytes / 1024.0_f64.powi(3)).floor() as u32
}

/// Check node labels for an SEV-SNP capability marker.
///
/// Looks for any label whose key ends with `sev-snp` or `sev_snp` and
/// whose value is `"true"`. Returns `false` if no such label is present
/// (the common case today — workers are not yet SEV-SNP-tagged).
fn has_sev_snp_label(node: &Node) -> bool {
    let labels = match node.metadata.labels.as_ref() {
        Some(l) => l,
        None => return false,
    };
    labels
        .iter()
        .any(|(k, v)| (k.ends_with("sev-snp") || k.ends_with("sev_snp")) && v == "true")
}

/// Return `true` if the node has a `Ready` condition with status `True`.
fn is_node_ready(node: &Node) -> bool {
    let conditions = match node.status.as_ref().and_then(|s| s.conditions.as_ref()) {
        Some(c) => c,
        None => return false,
    };
    conditions
        .iter()
        .any(|c| c.type_ == "Ready" && c.status == "True")
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{NodeCondition, NodeStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    // ---- quantity parsing -------------------------------------------------

    #[test]
    fn test_parse_cpu_quantity_plain() {
        assert_eq!(parse_cpu_quantity("4"), 4);
        assert_eq!(parse_cpu_quantity("16"), 16);
    }

    #[test]
    fn test_parse_cpu_quantity_milli() {
        assert_eq!(parse_cpu_quantity("4000m"), 4);
        assert_eq!(parse_cpu_quantity("4500m"), 4); // floored
        assert_eq!(parse_cpu_quantity("500m"), 0);
    }

    #[test]
    fn test_parse_cpu_quantity_fractional() {
        assert_eq!(parse_cpu_quantity("0.5"), 0);
        assert_eq!(parse_cpu_quantity("2.9"), 2);
    }

    #[test]
    fn test_parse_cpu_quantity_garbage() {
        assert_eq!(parse_cpu_quantity(""), 0);
        assert_eq!(parse_cpu_quantity("abc"), 0);
    }

    #[test]
    fn test_parse_memory_quantity_kib() {
        // 8,153,076 KiB ≈ 7.77 GiB → 7 GB (floored)
        assert_eq!(parse_memory_quantity("8153076Ki"), 7);
    }

    #[test]
    fn test_parse_memory_quantity_gib() {
        assert_eq!(parse_memory_quantity("16Gi"), 16);
        assert_eq!(parse_memory_quantity("8Gi"), 8);
    }

    #[test]
    fn test_parse_memory_quantity_mib() {
        // 16384 MiB = 16 GiB → 16 GB
        assert_eq!(parse_memory_quantity("16384Mi"), 16);
    }

    #[test]
    fn test_parse_memory_quantity_decimal_g() {
        // "16G" is 16 *decimal* gigabytes (16e9 bytes), which is ~14.9 GiB.
        // Our output is in binary GB (GiB), floored → 14.
        assert_eq!(parse_memory_quantity("16G"), 14);
        // "17G" = 17e9 bytes ≈ 15.83 GiB → 15.
        assert_eq!(parse_memory_quantity("17G"), 15);
    }

    #[test]
    fn test_parse_memory_quantity_bare_bytes() {
        // 8 GiB expressed as raw bytes
        assert_eq!(parse_memory_quantity("8589934592"), 8);
    }

    #[test]
    fn test_parse_memory_quantity_garbage() {
        assert_eq!(parse_memory_quantity(""), 0);
        assert_eq!(parse_memory_quantity("abc"), 0);
        assert_eq!(parse_memory_quantity("12ZZ"), 0);
    }

    // ---- node helpers -----------------------------------------------------

    fn node_with_labels(labels: &[(&str, &str)]) -> Node {
        let map: std::collections::BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Node {
            metadata: ObjectMeta {
                labels: Some(map),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        }
    }

    fn node_with_conditions(conds: &[(&str, &str)]) -> Node {
        let conditions: Vec<NodeCondition> = conds
            .iter()
            .map(|(t, s)| NodeCondition {
                type_: t.to_string(),
                status: s.to_string(),
                ..Default::default()
            })
            .collect();
        Node {
            metadata: Default::default(),
            spec: Default::default(),
            status: Some(NodeStatus {
                conditions: Some(conditions),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn test_has_sev_snp_label_false_without_labels() {
        let node = Node::default();
        assert!(!has_sev_snp_label(&node));
    }

    #[test]
    fn test_has_sev_snp_label_true_with_namespaced_key() {
        let node = node_with_labels(&[("stronghold.dev/sev-snp", "true")]);
        assert!(has_sev_snp_label(&node));
    }

    #[test]
    fn test_has_sev_snp_label_true_with_bare_key() {
        let node = node_with_labels(&[("sev_snp", "true")]);
        assert!(has_sev_snp_label(&node));
    }

    #[test]
    fn test_has_sev_snp_label_false_when_value_not_true() {
        let node = node_with_labels(&[("stronghold.dev/sev-snp", "false")]);
        assert!(!has_sev_snp_label(&node));
    }

    #[test]
    fn test_is_node_ready_no_status() {
        let node = Node::default();
        assert!(!is_node_ready(&node));
    }

    #[test]
    fn test_is_node_ready_true() {
        let node = node_with_conditions(&[("Ready", "True")]);
        assert!(is_node_ready(&node));
    }

    #[test]
    fn test_is_node_ready_false_status() {
        let node = node_with_conditions(&[("Ready", "False")]);
        assert!(!is_node_ready(&node));
    }

    #[test]
    fn test_is_node_ready_unknown_condition() {
        let node = node_with_conditions(&[("OutOfDisk", "True")]);
        assert!(!is_node_ready(&node));
    }

    #[test]
    fn test_is_node_ready_mixed_conditions() {
        let node = node_with_conditions(&[("OutOfDisk", "False"), ("Ready", "True")]);
        assert!(is_node_ready(&node));
    }

    #[test]
    fn test_parse_capacity_no_status() {
        let node = Node::default();
        assert_eq!(parse_capacity(&node), (0, 0));
    }

    // ---- integration-style tests (require k3s) ----------------------------
    //
    // These mirror the pattern in `scheduler.rs`: they skip gracefully when
    // no kubeconfig is available so `cargo test` passes in CI/dev boxes
    // without a cluster. On a real k3s deployment they exercise the live
    // API and verify the DOD.

    #[tokio::test]
    async fn test_list_with_real_k3s() {
        if get_kube_client().await.is_err() {
            eprintln!("Skipping k8s test: k3s/kubeconfig not available");
            return;
        }
        let workers = list().await.expect("list() should succeed with a client");
        // We don't assert non-empty — a fresh single-box deploy may list
        // only the control-plane node. The point is the call works and
        // returns a Vec<Worker>.
        println!("k3s workers: {:?}", workers);
        for w in &workers {
            // Host must always be populated.
            assert!(!w.host.is_empty(), "worker host must not be empty");
        }
    }

    /// DOD: `health_check()` returns false for a non-existent node.
    #[tokio::test]
    async fn test_health_check_nonexistent_node() {
        if get_kube_client().await.is_err() {
            eprintln!("Skipping k8s test: k3s/kubeconfig not available");
            return;
        }
        let healthy = health_check("definitely-not-a-real-node-xyz-pdq")
            .await
            .expect("health_check should not error on a 404");
        assert!(
            !healthy,
            "non-existent node must be reported as unhealthy (false)"
        );
    }
}
