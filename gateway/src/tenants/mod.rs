//! Multi-tenant registry, quotas, and authentication.
//!
//! Every database table has `tenant_id` as the first column. Every query
//! is tenant-scoped. No global state. No cross-tenant leakage possible
//! at the data layer.

pub mod auth;
pub mod quotas;
pub mod registry;
