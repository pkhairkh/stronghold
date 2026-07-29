//! Image management routes — build, push, list, inspect.
//!
//! These endpoints let agents (and the CLI) manage the Stronghold-internal
//! OCI registry without shelling out to `buildah`/`crictl` directly.
//!
//! ## Endpoints
//!
//! | Method | Path                            | Handler            |
//! |--------|---------------------------------|--------------------|
//! | POST   | /admin/images/build             | [`build_image`]    |
//! | POST   | /admin/images/push              | [`push_image`]     |
//! | POST   | /admin/images/pull              | [`pull_image`]     |
//! | GET    | /admin/images                   | [`list_images`]    |
//! | GET    | /admin/images/:name/tags        | [`list_tags`]      |
//! | GET    | /admin/images/:name/exists      | [`check_exists`]   |

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

// ============================================================================
// Request / response types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct BuildImageRequest {
    /// Image name (e.g. `"rocky-base"`, `"rust-nightly"`). Must match a
    /// directory under `images/` in the stronghold repo.
    pub name: String,
    /// Optional tag (defaults to `"latest"`).
    #[serde(default = "default_tag")]
    pub tag: String,
}

fn default_tag() -> String {
    "latest".to_string()
}

#[derive(Debug, Serialize)]
pub struct BuildImageResponse {
    pub image: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct PushImageRequest {
    /// Local image name (e.g. `stronghold/rocky-base:latest`).
    pub image: String,
    /// Registry endpoint. Defaults to the STRONGHOLD_REGISTRY env var
    /// (or `localhost:30500`).
    #[serde(default)]
    pub registry: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PushImageResponse {
    pub image: String,
    pub registry_ref: String,
    pub digest: String,
}

#[derive(Debug, Deserialize)]
pub struct PullImageRequest {
    /// Image reference (e.g. `stronghold/rocky-base:latest`).
    pub image: String,
}

#[derive(Debug, Serialize)]
pub struct PullImageResponse {
    pub image: String,
    pub digest: String,
}

#[derive(Debug, Serialize)]
pub struct ListImagesResponse {
    pub repositories: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ListTagsResponse {
    pub repository: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckExistsResponse {
    pub image: String,
    pub exists: bool,
}

// ============================================================================
// Handlers
// ============================================================================

/// `POST /admin/images/build` — build an image from its `image.toml` spec.
///
/// Reads `images/<name>/image.toml` from the stronghold repo, generates a
/// Containerfile, builds with `buildah`, and pushes to the registry.
pub async fn build_image(
    State(_state): State<AppState>,
    Json(req): Json<BuildImageRequest>,
) -> Result<Json<BuildImageResponse>, (StatusCode, String)> {
    tracing::info!(image = %req.name, tag = %req.tag, "Building image from spec");

    let image = crate::images::registry::build_from_spec(&req.name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(BuildImageResponse {
        image: image.clone(),
        status: "built".to_string(),
        message: format!("Image {} built and pushed to registry", image),
    }))
}

/// `POST /admin/images/push` — push a local image to the Stronghold registry.
pub async fn push_image(
    State(_state): State<AppState>,
    Json(req): Json<PushImageRequest>,
) -> Result<Json<PushImageResponse>, (StatusCode, String)> {
    tracing::info!(image = %req.image, "Pushing image to registry");

    let registry = req
        .registry
        .clone()
        .unwrap_or_else(crate::images::registry::registry_endpoint);

    let digest = crate::images::registry::push(&req.image, &registry)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let registry_ref = format!("{}/{}", registry, req.image);

    Ok(Json(PushImageResponse {
        image: req.image,
        registry_ref,
        digest,
    }))
}

/// `POST /admin/images/pull` — pull an image from the registry into k3s containerd.
pub async fn pull_image(
    State(_state): State<AppState>,
    Json(req): Json<PullImageRequest>,
) -> Result<Json<PullImageResponse>, (StatusCode, String)> {
    tracing::info!(image = %req.image, "Pulling image from registry");

    let digest = crate::images::registry::pull(&req.image)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(PullImageResponse {
        image: req.image,
        digest,
    }))
}

/// `GET /admin/images` — list all repositories in the Stronghold registry.
pub async fn list_images(
    State(_state): State<AppState>,
) -> Result<Json<ListImagesResponse>, (StatusCode, String)> {
    let repos = crate::images::registry::list_repositories()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Json(ListImagesResponse {
        repositories: repos,
    }))
}

/// `GET /admin/images/:name/tags` — list tags for a repository.
pub async fn list_tags(
    State(_state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ListTagsResponse>, (StatusCode, String)> {
    // Prepend stronghold/ if the caller passed just the image name.
    let repo = if name.contains('/') {
        name.clone()
    } else {
        format!("stronghold/{}", name)
    };
    let tags = crate::images::registry::list_tags(&repo)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Json(ListTagsResponse {
        repository: name,
        tags,
    }))
}

/// `GET /admin/images/:name/exists` — check if an image exists in the registry.
pub async fn check_exists(
    State(_state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<CheckExistsResponse>, (StatusCode, String)> {
    let image_ref = format!("stronghold/{}:latest", name);
    let exists = crate::images::registry::exists(&image_ref)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Json(CheckExistsResponse {
        image: image_ref,
        exists,
    }))
}

// ============================================================================
// Router
// ============================================================================

/// Build the axum sub-router for all image-management endpoints.
pub fn image_routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::<AppState>::new()
        .route("/admin/images/build", post(build_image))
        .route("/admin/images/push", post(push_image))
        .route("/admin/images/pull", post(pull_image))
        .route("/admin/images", get(list_images))
        .route("/admin/images/:name/tags", get(list_tags))
        .route("/admin/images/:name/exists", get(check_exists))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_request_deserialize() {
        let json = r#"{"name":"rocky-base","tag":"latest"}"#;
        let req: BuildImageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "rocky-base");
        assert_eq!(req.tag, "latest");
    }

    #[test]
    fn test_build_request_default_tag() {
        let json = r#"{"name":"rust-nightly"}"#;
        let req: BuildImageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tag, "latest");
    }
}
