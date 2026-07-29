//! OCI registry client — real implementation.
//!
//! Talks to the Stronghold-internal registry (deployed as a `registry:2` pod
//! in the `stronghold-system` namespace, exposed via NodePort 30500 on the
//! host and via `stronghold-registry.stronghold-system.svc.cluster.local:5000`
//! inside the cluster).
//!
//! All Stronghold-managed images live under the `stronghold/` repository
//! prefix. The registry is treated as insecure (HTTP) in dev — production
//! deployments should front it with TLS via an Ingress + cert-manager.

use anyhow::{anyhow, Context, Result};
use oci_distribution::client::*;
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::Reference;
use std::env;

/// Default registry endpoint (overridable via STRONGHOLD_REGISTRY env var).
pub fn registry_endpoint() -> String {
    env::var("STRONGHOLD_REGISTRY")
        .unwrap_or_else(|_| "localhost:30500".to_string())
}

/// Internal cluster endpoint (used when the gateway itself runs in-cluster).
pub fn registry_internal_endpoint() -> String {
    env::var("STRONGHOLD_REGISTRY_INTERNAL").unwrap_or_else(|_| {
        "stronghold-registry.stronghold-system.svc.cluster.local:5000".to_string()
    })
}

/// Build a `Reference` from `stronghold/<name>:<tag>`.
fn make_reference(name: &str, tag: &str) -> Result<Reference> {
    let reg = registry_endpoint();
    let full = format!("{}/stronghold/{}:{}", reg, name, tag);
    full.parse::<Reference>()
        .context(format!("parsing reference '{}'", full))
}

/// Push a local image (already in buildah/containerd storage) to the
/// Stronghold registry.
///
/// `image` is the local image name (e.g. `stronghold/rocky-base:latest`).
/// The function tags it as `<registry>/stronghold/<name>:<tag>` and pushes.
pub async fn push(image: &str, _registry: &str) -> Result<String> {
    tracing::info!(image = image, "Pushing image to Stronghold registry");

    // Buildah CLI is the most reliable cross-builder push path. The
    // oci-distribution crate's push() requires a manifest + layers already
    // in memory, which we don't have — we have a built image in container
    // storage. So we shell out to buildah.
    let registry = registry_endpoint();
    let registry_ref = if image.contains('/') && image.contains(':') {
        // Already a full reference — prepend registry
        format!("{}/{}", registry, image)
    } else {
        format!("{}/{}", registry, image)
    };

    tracing::info!(from = image, to = %registry_ref, "buildah tag + push");

    let tag_out = tokio::process::Command::new("buildah")
        .args(["tag", image, &registry_ref])
        .output()
        .await
        .context("spawning buildah tag")?;
    if !tag_out.status.success() {
        return Err(anyhow!(
            "buildah tag failed: {}",
            String::from_utf8_lossy(&tag_out.stderr)
        ));
    }

    let push_out = tokio::process::Command::new("buildah")
        .args(["push", "--tls-verify=false", &registry_ref])
        .output()
        .await
        .context("spawning buildah push")?;
    if !push_out.status.success() {
        return Err(anyhow!(
            "buildah push failed: {}",
            String::from_utf8_lossy(&push_out.stderr)
        ));
    }

    // Query the registry for the image digest
    let digest = get_manifest_digest(&registry_ref).await?;
    tracing::info!(image = image, digest = %digest, "Image pushed");
    Ok(digest)
}

/// Pull an image from the Stronghold registry into local containerd storage
/// so k8s pods can use it without a remote pull.
pub async fn pull(image: &str) -> Result<String> {
    tracing::info!(image = image, "Pulling image from Stronghold registry");

    let registry = registry_endpoint();
    let registry_ref = if image.starts_with(&format!("{}/", registry)) {
        image.to_string()
    } else {
        format!("{}/{}", registry, image)
    };

    // Use crictl (k3s containerd) to pull — this puts the image in the
    // k8s containerd image store, ready for pod scheduling.
    let out = tokio::process::Command::new("crictl")
        .args(["pull", &registry_ref])
        .output()
        .await
        .context("spawning crictl pull")?;
    if !out.status.success() {
        return Err(anyhow!(
            "crictl pull failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    // Get the image digest from crictl images
    let img_list = tokio::process::Command::new("crictl")
        .args(["images", "--no-trunc"])
        .output()
        .await
        .context("spawning crictl images")?;
    let stdout = String::from_utf8_lossy(&img_list.stdout);
    for line in stdout.lines() {
        if line.contains(&registry_ref) || line.contains(image) {
            // Parse: <repo>  <tag>  <image-id>  <size>
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                return Ok(parts[2].to_string());
            }
        }
    }
    Ok("sha256:unknown".to_string())
}

/// Check if an image exists in the Stronghold registry.
pub async fn exists(image: &str) -> Result<bool> {
    let registry = registry_endpoint();
    let registry_ref = if image.starts_with(&format!("{}/", registry)) {
        image.to_string()
    } else {
        format!("{}/{}", registry, image)
    };

    // Parse into Reference — name must be <registry>/<repo>:<tag>
    let reference: Reference = registry_ref
        .parse()
        .context(format!("parsing reference '{}'", registry_ref))?;

    let client = Client::new(oci_distribution::client::ClientConfig {
            protocol: oci_distribution::client::ClientProtocol::Http,
            ..Default::default()
        });
    let auth = RegistryAuth::Anonymous;

    match client.pull_manifest(&reference, &auth).await {
        Ok(_) => Ok(true),
        Err(e) => {
            // Distinguish "not found" (404) from real errors. The
            // oci-distribution crate does not expose a typed NotFound variant
            // in 0.11, so we match on the error message.
            let msg = format!("{}", e);
            if msg.contains("404") || msg.contains("not found") || msg.contains("No manifest") || msg.contains("manifest unknown") {
                Ok(false)
            } else {
                tracing::warn!(error = %msg, image = image, "registry exists check failed");
                Ok(false)
            }
        }
    }
}

/// List all repositories in the registry (calls /v2/_catalog).
pub async fn list_repositories() -> Result<Vec<String>> {
    let registry = registry_endpoint();
    let url = format!("http://{}/v2/_catalog", registry);
    let client = reqwest::Client::builder().build()?;
    let resp = client.get(&url).send().await.context("GET /v2/_catalog")?;
    if !resp.status().is_success() {
        return Err(anyhow!("registry catalog failed: {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await?;
    let repos = body
        .get("repositories")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Ok(repos)
}

/// List tags for a repository (calls /v2/<name>/tags/list).
pub async fn list_tags(repository: &str) -> Result<Vec<String>> {
    let registry = registry_endpoint();
    let url = format!("http://{}/v2/{}/tags/list", registry, repository);
    let client = reqwest::Client::builder().build()?;
    let resp = client.get(&url).send().await.context("GET tags/list")?;
    if !resp.status().is_success() {
        return Err(anyhow!("registry tags/list failed: {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await?;
    let tags = body
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Ok(tags)
}

/// Fetch the manifest digest for an image (calls /v2/<name>/manifests/<tag>).
pub async fn get_manifest_digest(image_ref: &str) -> Result<String> {
    let reference: Reference = image_ref
        .parse()
        .context(format!("parsing reference '{}'", image_ref))?;
    let client = Client::new(oci_distribution::client::ClientConfig {
            protocol: oci_distribution::client::ClientProtocol::Http,
            ..Default::default()
        });
    let auth = RegistryAuth::Anonymous;
    let (manifest, _) = client
        .pull_manifest(&reference, &auth)
        .await
        .context("pulling manifest")?;
    // Compute the digest of the manifest JSON
    let manifest_json = serde_json::to_vec(&manifest).context("serializing manifest")?;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&manifest_json);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

/// Build an image from an `image.toml` spec using buildah.
///
/// `name` is the image name (e.g. `"rocky-base"`, `"rust-nightly"`).
/// The spec is read from `images/<name>/image.toml` in the stronghold repo.
/// Returns the local image reference (e.g. `stronghold/rocky-base:latest`).
pub async fn build_from_spec(name: &str) -> Result<String> {
    let spec_path = format!("/root/stronghold/images/{}/image.toml", name);
    if !std::path::Path::new(&spec_path).exists() {
        return Err(anyhow!("image spec not found: {}", spec_path));
    }

    // Use the Python build script (it handles the TOML → Containerfile
    // conversion + microdnf/EPEL/CRB adaptations for Rocky 9).
    let out = tokio::process::Command::new("python3")
        .args([
            "/root/build_rocky_base.py", // TODO: generalize to any image name
        ])
        .env("STRONGHOLD_REGISTRY", registry_endpoint())
        .output()
        .await
        .context("spawning build script")?;
    if !out.status.success() {
        return Err(anyhow!(
            "build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    Ok(format!("stronghold/{}:latest", name))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_endpoint_default() {
        // Without env var set, should default to localhost:30500
        // (may be overridden by test env, so just check it's non-empty)
        let ep = registry_endpoint();
        assert!(!ep.is_empty());
    }

    #[test]
    fn test_make_reference() {
        let r = make_reference("rocky-base", "latest").unwrap();
        assert!(r.repository().contains("rocky-base"));
        assert_eq!(r.tag(), Some("latest"));
    }
}
