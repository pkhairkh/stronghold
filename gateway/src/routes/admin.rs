//! Admin endpoints — tenant management.
//!
//! These endpoints are used by the `stronghold` CLI to manage tenants,
//! quotas, and configuration. They are authenticated via admin token
//! (separate from agent tokens).

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateTenantRequest {
    pub name: String,
    #[serde(default)]
    pub max_concurrent_machines: Option<u32>,
    #[serde(default)]
    pub max_cpu_per_machine: Option<u32>,
    #[serde(default)]
    pub max_memory_gb_per_machine: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TenantResponse {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub setup_password: String,
    pub enrollment_url: String,
    pub sev_snp_measurement: String,
}

/// Create a new tenant.
pub async fn create_tenant(
    State(state): State<AppState>,
    Json(req): Json<CreateTenantRequest>,
) -> Result<Json<TenantResponse>, (StatusCode, String)> {
    tracing::info!("Creating tenant: {}", req.name);

    let tenant = crate::tenants::registry::create(&state.db, &req.name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Set quotas if provided
    if let Some(max_machines) = req.max_concurrent_machines {
        crate::tenants::quotas::set(
            &state.db,
            &tenant.id,
            max_machines,
            req.max_cpu_per_machine.unwrap_or(4),
            req.max_memory_gb_per_machine.unwrap_or(8),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    Ok(Json(TenantResponse {
        id: tenant.id.clone(),
        name: tenant.name.clone(),
        created_at: tenant.created_at,
        setup_password: tenant.setup_password,
        enrollment_url: format!("/setup?tenant={}", tenant.id),
        sev_snp_measurement: crate::tee::current_measurement().unwrap_or_default(),
    }))
}

/// Get tenant info.
pub async fn get_tenant(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TenantResponse>, (StatusCode, String)> {
    let tenant = crate::tenants::registry::get(&state.db, &id)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(TenantResponse {
        id: tenant.id,
        name: tenant.name,
        created_at: tenant.created_at,
        setup_password: "[redacted]".to_string(),
        enrollment_url: format!("/setup?tenant={}", id),
        sev_snp_measurement: crate::tee::current_measurement().unwrap_or_default(),
    }))
}
