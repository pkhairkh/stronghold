//! OCI registry client — push/pull images from ghcr.io or private registry.

use anyhow::Result;

/// Push an image to a registry.
pub async fn push(image: &str, registry: &str) -> Result<String> {
    tracing::info!(image = image, registry = registry, "Pushing image (stub)");

    // TODO: use oci-distribution crate to push
    let digest = format!("sha256:{}", hex::encode(&[0u8; 32]));
    Ok(digest)
}

/// Pull an image from a registry.
pub async fn pull(image: &str) -> Result<String> {
    tracing::info!(image = image, "Pulling image (stub)");

    // TODO: use oci-distribution crate to pull
    let digest = format!("sha256:{}", hex::encode(&[0u8; 32]));
    Ok(digest)
}

/// Check if an image exists in a registry.
pub async fn exists(image: &str) -> Result<bool> {
    tracing::debug!(image = image, "Checking image existence (stub)");
    Ok(true)
}
