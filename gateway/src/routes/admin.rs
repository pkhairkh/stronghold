//! Admin endpoints — tenant management, agent roles, and constitution.
//!
//! These endpoints are used by the `stronghold` CLI to manage tenants,
//! quotas, roles, and configuration. They are authenticated via admin token
//! (separate from agent tokens).
//!
//! ## Role + constitution endpoints
//!
//! The handler implementations live in the [`roles`] submodule (sourced from
//! `routes/roles.rs` via the `#[path]` attribute, so `routes/mod.rs` stays
//! untouched — the orchestrator wires the routes later). This module
//! re-exports them under the `admin::` namespace and provides
//! [`role_routes`] — a ready-to-merge [`axum::Router`] that wires every
//! role + constitution URL. The orchestrator is expected to merge this into
//! the main router:
//!
//! ```ignore
//! Router::new()
//!     .merge(admin::role_routes())
//!     .with_state(state)
//! ```

use crate::routes::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Role + constitution module
// ---------------------------------------------------------------------------
//
// `roles.rs` lives at `gateway/src/routes/roles.rs` (a sibling of `admin.rs`).
// We pull it in as a private submodule of `admin` via the `#[path]` attribute
// rather than declaring `pub mod roles;` in `routes/mod.rs` — this keeps
// `mod.rs` untouched (the orchestrator wires routes there separately).
// Everything in `roles` is re-exported under `admin::` so callers can address
// handlers as either `admin::create_role` or `admin::roles::create_role`.

#[path = "roles.rs"]
mod roles;

// `pub use` re-exports so callers can address role handlers as either
// `crate::routes::admin::create_role` or `crate::routes::admin::roles::create_role`.
// These are part of the module's public API — the `unused_imports` warning
// would be a false positive (the items are consumed by external callers, not
// internally), so we suppress it.
#[allow(unused_imports)]
pub use roles::{
    check_tool_allowed, constitution, create_role, delete_role, get_constitution, get_role,
    list_roles, seed_roles, ConstitutionPrinciple, CreateRoleRequest, CreateRoleResponse,
    GetRoleResponse, ListRoleItem, ListRolesQuery, SeedRolesRequest, SeedRolesResponse,
};

/// Build the axum sub-router for all role + constitution endpoints.
///
/// Returns a `Router<AppState>` — the caller merges it into the main router
/// and applies `.with_state(state)` once at the end:
///
/// ```ignore
/// let router = Router::new()
///     // ...other routes...
///     .merge(admin::role_routes())
///     .with_state(state);
/// ```
///
/// Routes wired:
/// - `POST   /admin/roles`           — [`create_role`] + [`list_roles`] (GET)
/// - `POST   /admin/roles/seed`      — [`seed_roles`]
/// - `GET    /admin/roles/:id`       — [`get_role`]
/// - `DELETE /admin/roles/:id`       — [`delete_role`]
/// - `GET    /admin/constitution`    — [`get_constitution`]
pub fn role_routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::<AppState>::new()
        .route(
            "/admin/roles",
            post(roles::create_role).get(roles::list_roles),
        )
        .route("/admin/roles/seed", post(roles::seed_roles))
        .route(
            "/admin/roles/:id",
            get(roles::get_role).delete(roles::delete_role),
        )
        .route("/admin/constitution", get(roles::get_constitution))
}

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
