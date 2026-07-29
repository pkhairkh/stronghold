#![allow(clippy::doc_overindented_list_items)]
//! Credential vault CRUD endpoints + agent credential access.
//!
//! Implemented in: K2 (CRUD) + K3 (agent access)
//!
//! Admin endpoints (CLI-facing, tenant-scoped by request body / path):
//! - `POST   /admin/credentials`           — Store an encrypted credential
//! - `GET    /admin/credentials?tenant=…`  — List credential metadata (no values)
//! - `GET    /admin/credentials/:id`       — Fetch + DECRYPT a single credential
//! - `DELETE /admin/credentials/:id`       — Delete a credential
//! - `POST   /admin/credentials/:id/rotate`— Rotate the encrypted value
//!
//! Agent endpoint (machine-scoped, bearer-token authenticated):
//! - `GET    /agent/:machine_id/credentials/:name` — Fetch a named credential
//!   for the machine's tenant. Writes an audit entry; **never** logs the value.
//!
//! All credential values are encrypted at rest with a per-tenant AES-256-GCM
//! key derived (via HKDF-256) from the audit Ed25519 secret key + tenant_id.
//! The tenant key is derived on demand and never persisted. See
//! [`crate::crypto::vault`] for the crypto details.

use crate::crypto::vault;
use crate::routes::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

// ============================================================================
// Request / response types
// ============================================================================

/// Request body for `POST /admin/credentials`.
#[derive(Debug, Deserialize)]
pub struct CreateCredentialRequest {
    /// Tenant the credential belongs to.
    pub tenant_id: String,
    /// Human-readable name, unique per tenant (e.g. `"github_token"`).
    pub name: String,
    /// Credential kind: `ssh_key`, `api_token`, `env_var`, or `file`.
    pub kind: String,
    /// Plaintext secret value. Encrypted at rest; never logged.
    pub value: String,
    /// When set, the agent injects the decrypted value as this env var
    /// (e.g. `"GITHUB_TOKEN"`).
    #[serde(default)]
    pub env_var: Option<String>,
    /// When set, the agent writes the decrypted value to this path inside
    /// the container (e.g. `"/home/dev/.ssh/id_ed25519"`).
    #[serde(default)]
    pub mount_path: Option<String>,
}

/// Response body for `POST /admin/credentials`.
#[derive(Debug, Serialize)]
pub struct CreateCredentialResponse {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub created_at: String,
}

/// Query string for `GET /admin/credentials?tenant=<id>`.
#[derive(Debug, Deserialize)]
pub struct ListCredentialsQuery {
    pub tenant: String,
}

/// One row in the `GET /admin/credentials?tenant=…` list response.
///
/// Metadata only — the encrypted value is **never** included.
#[derive(Debug, Serialize)]
pub struct ListCredentialItem {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub env_var: Option<String>,
    pub mount_path: Option<String>,
    pub created_at: String,
    pub rotated_at: Option<String>,
}

/// Response body for `GET /admin/credentials/:id` (decrypts the value).
#[derive(Debug, Serialize)]
pub struct GetCredentialResponse {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub value: String,
    pub env_var: Option<String>,
    pub mount_path: Option<String>,
}

/// Request body for `POST /admin/credentials/:id/rotate`.
#[derive(Debug, Deserialize)]
pub struct RotateCredentialRequest {
    /// New plaintext secret value. Re-encrypted with the same tenant key.
    pub value: String,
}

/// Response body for `POST /admin/credentials/:id/rotate`.
#[derive(Debug, Serialize)]
pub struct RotateCredentialResponse {
    pub id: String,
    pub name: String,
    pub rotated_at: String,
}

/// Response body for `GET /agent/:machine_id/credentials/:name`.
///
/// Mirrors [`GetCredentialResponse`] minus the `id` field (agents identify
/// credentials by name within their machine's tenant).
#[derive(Debug, Serialize)]
pub struct AgentCredentialResponse {
    pub name: String,
    pub kind: String,
    pub value: String,
    pub env_var: Option<String>,
    pub mount_path: Option<String>,
}

// ============================================================================
// Handlers — admin
// ============================================================================

/// `POST /admin/credentials` — store an encrypted credential.
///
/// Derives the per-tenant AES-256-GCM key from the audit Ed25519 secret key
/// and `tenant_id`, encrypts `value`, and persists the ciphertext + nonce to
/// the `agent_credentials` table. The plaintext value is never stored or
/// logged.
pub async fn create_credential(
    State(state): State<AppState>,
    Json(req): Json<CreateCredentialRequest>,
) -> Result<Json<CreateCredentialResponse>, (StatusCode, String)> {
    // Derive the per-tenant key and encrypt the plaintext value.
    let tenant_key = vault::derive_tenant_key(&req.tenant_id, &state.audit_keys);
    let (ciphertext, nonce) = vault::encrypt(req.value.as_bytes(), &tenant_key)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let id = ulid::Ulid::new().to_string();
    let created_at = chrono::Utc::now().to_rfc3339();

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    conn.execute(
        "INSERT INTO agent_credentials
         (id, tenant_id, name, kind, encrypted_value, nonce,
          env_var, mount_path, created_at, rotated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
        rusqlite::params![
            id,
            req.tenant_id,
            req.name,
            req.kind,
            ciphertext,
            nonce,
            req.env_var,
            req.mount_path,
            created_at,
        ],
    )
    .map_err(|e| {
        // UNIQUE(tenant_id, name) violation → 409 Conflict.
        if let rusqlite::Error::SqliteFailure(ref f, _) = e {
            if f.extended_code == 2067 /* SQLITE_CONSTRAINT_UNIQUE */ {
                return (
                    StatusCode::CONFLICT,
                    format!(
                        "Credential with name '{}' already exists for tenant '{}'",
                        req.name, req.tenant_id
                    ),
                );
            }
        }
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    tracing::info!(
        tenant = %req.tenant_id,
        cred_id = %id,
        name = %req.name,
        kind = %req.kind,
        "Credential stored"
    );

    Ok(Json(CreateCredentialResponse {
        id,
        name: req.name,
        kind: req.kind,
        created_at,
    }))
}

/// `GET /admin/credentials?tenant=<id>` — list credential metadata.
///
/// Returns all credentials for the given tenant, **metadata only**. The
/// encrypted value and nonce are never included in the response.
pub async fn list_credentials(
    State(state): State<AppState>,
    Query(q): Query<ListCredentialsQuery>,
) -> Result<Json<Vec<ListCredentialItem>>, (StatusCode, String)> {
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, name, kind, env_var, mount_path, created_at, rotated_at
             FROM agent_credentials
             WHERE tenant_id = ?1
             ORDER BY created_at ASC",
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<ListCredentialItem> = stmt
        .query_map(rusqlite::params![q.tenant], |row| {
            Ok(ListCredentialItem {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                env_var: row.get(3)?,
                mount_path: row.get(4)?,
                created_at: row.get(5)?,
                rotated_at: row.get(6)?,
            })
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(
        tenant = %q.tenant,
        count = rows.len(),
        "Credential metadata listed"
    );

    Ok(Json(rows))
}

/// `GET /admin/credentials/:id` — fetch + decrypt a single credential.
///
/// Decrypts the value and writes a `credential_accessed` audit entry whose
/// payload records `{name}` (never the value). Returns the decrypted value
/// in the response.
pub async fn get_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<GetCredentialResponse>, (StatusCode, String)> {
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    // Read the row.
    let row = conn.query_row(
        "SELECT tenant_id, name, kind, encrypted_value, nonce, env_var, mount_path
         FROM agent_credentials
         WHERE id = ?1",
        rusqlite::params![id],
        |row| {
            let ciphertext: Vec<u8> = row.get(3)?;
            let nonce: Vec<u8> = row.get(4)?;
            Ok((
                row.get::<_, String>(0)?, // tenant_id
                row.get::<_, String>(1)?, // name
                row.get::<_, String>(2)?, // kind
                ciphertext,
                nonce,
                row.get::<_, Option<String>>(5)?, // env_var
                row.get::<_, Option<String>>(6)?, // mount_path
            ))
        },
    );

    let (tenant_id, name, kind, ciphertext, nonce, env_var, mount_path) = match row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("Credential not found: {}", id),
            ));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };

    // Decrypt with the per-tenant key.
    let tenant_key = vault::derive_tenant_key(&tenant_id, &state.audit_keys);
    let plaintext = vault::decrypt(&ciphertext, &nonce, &tenant_key)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let value = String::from_utf8(plaintext)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Audit entry — payload carries only the name, NEVER the value.
    let audit_payload = serde_json::json!({ "name": name });
    if let Err(e) = crate::audit::log::entry(
        &state.db,
        &tenant_id,
        "", // admin access — no machine_id
        "credential_accessed",
        audit_payload,
        &state.audit_keys,
    ) {
        tracing::error!(error = %e, cred_id = %id, "Failed to write credential_accessed audit entry");
    }

    tracing::info!(cred_id = %id, tenant = %tenant_id, "Credential decrypted (admin access)");

    Ok(Json(GetCredentialResponse {
        id,
        name,
        kind,
        value,
        env_var,
        mount_path,
    }))
}

/// `DELETE /admin/credentials/:id` — delete a credential.
///
/// Returns `204 No Content` on success, `404 Not Found` if the credential
/// does not exist.
pub async fn delete_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    let affected = conn
        .execute(
            "DELETE FROM agent_credentials WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if affected == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Credential not found: {}", id),
        ));
    }

    tracing::info!(cred_id = %id, "Credential deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /admin/credentials/:id/rotate` — rotate a credential's value.
///
/// Re-encrypts the new plaintext value with the **same** tenant key (the
/// tenant key is deterministic given the audit keys + tenant_id), updates
/// `encrypted_value` / `nonce`, and stamps `rotated_at` with the current
/// RFC 3339 timestamp. Returns the new `rotated_at` so the caller can
/// confirm the rotation took effect.
pub async fn rotate_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RotateCredentialRequest>,
) -> Result<Json<RotateCredentialResponse>, (StatusCode, String)> {
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    // Fetch tenant_id + name so we can derive the same tenant key and report
    // the name back in the response. A missing row → 404.
    let (tenant_id, name): (String, String) = match conn.query_row(
        "SELECT tenant_id, name FROM agent_credentials WHERE id = ?1",
        rusqlite::params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ) {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("Credential not found: {}", id),
            ));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };

    // Re-encrypt with the same tenant key.
    let tenant_key = vault::derive_tenant_key(&tenant_id, &state.audit_keys);
    let (ciphertext, nonce) = vault::encrypt(req.value.as_bytes(), &tenant_key)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rotated_at = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "UPDATE agent_credentials
         SET encrypted_value = ?1, nonce = ?2, rotated_at = ?3
         WHERE id = ?4",
        rusqlite::params![ciphertext, nonce, rotated_at, id],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(cred_id = %id, tenant = %tenant_id, "Credential rotated");

    Ok(Json(RotateCredentialResponse {
        id,
        name,
        rotated_at,
    }))
}

// ============================================================================
// Handlers — agent
// ============================================================================

/// `GET /agent/:machine_id/credentials/:name` — agent fetches a named
/// credential for its machine's tenant.
///
/// Flow:
/// 1. Verify the agent bearer token → `tenant_id` (from `agent_tokens`).
/// 2. Look up the machine's `tenant_id` from the `machines` table and
///    confirm it matches the token's tenant (cross-tenant access → 403).
/// 3. Query `agent_credentials WHERE tenant_id = ? AND name = ?`.
/// 4. Decrypt the value.
/// 5. Write a `credential_accessed` audit entry whose payload records
///    `{name, machine_id}` (NEVER the value).
/// 6. Return `{ name, kind, value, env_var, mount_path }`.
pub async fn agent_get_credential(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path((machine_id, name)): Path<(String, String)>,
) -> Result<Json<AgentCredentialResponse>, (StatusCode, String)> {
    // --- Step 1: verify the agent bearer token. ---
    let agent_token = extract_token(&headers)?;
    let token_tenant_id = crate::tenants::auth::verify_agent_token(&state.db, &agent_token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    // --- Step 2: look up the machine's tenant_id and confirm it matches. ---
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let machine_tenant_id: String = match conn.query_row(
        "SELECT tenant_id FROM machines WHERE id = ?1",
        rusqlite::params![machine_id],
        |row| row.get(0),
    ) {
        Ok(t) => t,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("Machine not found: {}", machine_id),
            ));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };

    if machine_tenant_id != token_tenant_id {
        tracing::warn!(
            token_tenant = %token_tenant_id,
            machine_tenant = %machine_tenant_id,
            machine = %machine_id,
            "Agent token tenant does not match machine tenant — denying credential access"
        );
        return Err((
            StatusCode::FORBIDDEN,
            "Agent token tenant does not match machine tenant".to_string(),
        ));
    }

    // The credential is scoped to the machine's (= token's) tenant.
    let tenant_id = machine_tenant_id;

    // --- Step 3: query the credential row. ---
    let row = conn.query_row(
        "SELECT kind, encrypted_value, nonce, env_var, mount_path
         FROM agent_credentials
         WHERE tenant_id = ?1 AND name = ?2",
        rusqlite::params![tenant_id, name],
        |row| {
            let ciphertext: Vec<u8> = row.get(1)?;
            let nonce: Vec<u8> = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?, // kind
                ciphertext,
                nonce,
                row.get::<_, Option<String>>(3)?, // env_var
                row.get::<_, Option<String>>(4)?, // mount_path
            ))
        },
    );

    let (kind, ciphertext, nonce, env_var, mount_path) = match row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("Credential not found: {}", name),
            ));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };

    // Release the pooled connection — we're done with the DB after the read.
    // (The audit write below acquires its own connection.)
    drop(conn);

    // --- Step 4: decrypt. ---
    let tenant_key = vault::derive_tenant_key(&tenant_id, &state.audit_keys);
    let plaintext = vault::decrypt(&ciphertext, &nonce, &tenant_key)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let value = String::from_utf8(plaintext)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // --- Step 5: audit entry — payload carries {name, machine_id}, NEVER the value. ---
    let audit_payload = serde_json::json!({
        "name": name,
        "machine_id": machine_id,
    });
    if let Err(e) = crate::audit::log::entry(
        &state.db,
        &tenant_id,
        &machine_id,
        "credential_accessed",
        audit_payload,
        &state.audit_keys,
    ) {
        tracing::error!(
            error = %e,
            machine = %machine_id,
            cred_name = %name,
            "Failed to write credential_accessed audit entry"
        );
    }

    tracing::info!(
        tenant = %tenant_id,
        machine = %machine_id,
        cred_name = %name,
        "Credential decrypted (agent access)"
    );

    // --- Step 6: respond (no `id` — agents address by name). ---
    Ok(Json(AgentCredentialResponse {
        name,
        kind,
        value,
        env_var,
        mount_path,
    }))
}

// ============================================================================
// Helpers
// ============================================================================

/// Extract a `Bearer <token>` from the `Authorization` header.
///
/// Mirrors the private helper in `routes/agent.rs` — returns `401` on a
/// missing or malformed header.
fn extract_token(headers: &axum::http::HeaderMap) -> Result<String, (StatusCode, String)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header".to_string(),
        ))?;

    if !auth.starts_with("Bearer ") {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Expected Bearer token".to_string(),
        ));
    }

    Ok(auth[7..].to_string())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- CreateCredentialRequest deserialization ---

    #[test]
    fn test_create_request_deserialize_full() {
        let json = r#"{
            "tenant_id": "tenant_01H",
            "name": "github_token",
            "kind": "api_token",
            "value": "ghp_secret123",
            "env_var": "GITHUB_TOKEN",
            "mount_path": null
        }"#;
        let req: CreateCredentialRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tenant_id, "tenant_01H");
        assert_eq!(req.name, "github_token");
        assert_eq!(req.kind, "api_token");
        assert_eq!(req.value, "ghp_secret123");
        assert_eq!(req.env_var.as_deref(), Some("GITHUB_TOKEN"));
        assert!(req.mount_path.is_none());
    }

    #[test]
    fn test_create_request_deserialize_minimal() {
        // Only the required fields; env_var / mount_path default to None.
        let json = r#"{
            "tenant_id": "t1",
            "name": "ssh_key",
            "kind": "ssh_key",
            "value": "-----BEGIN OPENSSH PRIVATE KEY-----"
        }"#;
        let req: CreateCredentialRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tenant_id, "t1");
        assert_eq!(req.name, "ssh_key");
        assert_eq!(req.kind, "ssh_key");
        assert!(req.env_var.is_none());
        assert!(req.mount_path.is_none());
    }

    #[test]
    fn test_create_request_deserialize_with_mount_path() {
        let json = r#"{
            "tenant_id": "t1",
            "name": "id_ed25519",
            "kind": "file",
            "value": "keybytes",
            "mount_path": "/home/dev/.ssh/id_ed25519"
        }"#;
        let req: CreateCredentialRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.mount_path.as_deref(), Some("/home/dev/.ssh/id_ed25519"));
        assert!(req.env_var.is_none());
    }

    #[test]
    fn test_create_request_missing_tenant_fails() {
        let json = r#"{"name":"n","kind":"k","value":"v"}"#;
        assert!(serde_json::from_str::<CreateCredentialRequest>(json).is_err());
    }

    #[test]
    fn test_create_request_missing_value_fails() {
        let json = r#"{"tenant_id":"t","name":"n","kind":"k"}"#;
        assert!(serde_json::from_str::<CreateCredentialRequest>(json).is_err());
    }

    // --- CreateCredentialResponse serialization ---

    #[test]
    fn test_create_response_serialize() {
        let resp = CreateCredentialResponse {
            id: "cred_01H".to_string(),
            name: "github_token".to_string(),
            kind: "api_token".to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":\"cred_01H\""), "json: {json}");
        assert!(json.contains("\"name\":\"github_token\""), "json: {json}");
        assert!(json.contains("\"kind\":\"api_token\""), "json: {json}");
        assert!(
            json.contains("\"created_at\":\"2025-01-01T00:00:00Z\""),
            "json: {json}"
        );
    }

    #[test]
    fn test_create_response_field_names_match_dod() {
        let resp = CreateCredentialResponse {
            id: "i".to_string(),
            name: "n".to_string(),
            kind: "k".to_string(),
            created_at: "t".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 4, "expected exactly 4 fields, got {obj:?}");
        assert!(obj.contains_key("id"));
        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("kind"));
        assert!(obj.contains_key("created_at"));
    }

    // --- ListCredentialsQuery deserialization ---

    #[test]
    fn test_list_query_deserialize() {
        // Mimics `?tenant=tenant_01H`.
        let json = r#"{"tenant":"tenant_01H"}"#;
        let q: ListCredentialsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.tenant, "tenant_01H");
    }

    #[test]
    fn test_list_query_missing_tenant_fails() {
        let json = r#"{}"#;
        assert!(serde_json::from_str::<ListCredentialsQuery>(json).is_err());
    }

    // --- ListCredentialItem serialization ---

    #[test]
    fn test_list_item_serialize_with_all_fields() {
        let item = ListCredentialItem {
            id: "cred_01H".to_string(),
            name: "github_token".to_string(),
            kind: "api_token".to_string(),
            env_var: Some("GITHUB_TOKEN".to_string()),
            mount_path: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            rotated_at: Some("2025-02-01T00:00:00Z".to_string()),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"id\":\"cred_01H\""), "json: {json}");
        assert!(json.contains("\"env_var\":\"GITHUB_TOKEN\""), "json: {json}");
        assert!(json.contains("\"mount_path\":null"), "json: {json}");
        assert!(
            json.contains("\"rotated_at\":\"2025-02-01T00:00:00Z\""),
            "json: {json}"
        );
    }

    #[test]
    fn test_list_item_serialize_never_includes_value() {
        // The ListCredentialItem struct must NOT have a `value` field at all —
        // verify by parsing the JSON and confirming the absence of the key.
        let item = ListCredentialItem {
            id: "i".to_string(),
            name: "n".to_string(),
            kind: "k".to_string(),
            env_var: None,
            mount_path: None,
            created_at: "t".to_string(),
            rotated_at: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("value"), "list response must not include value");
        assert!(!obj.contains_key("encrypted_value"));
        assert!(!obj.contains_key("nonce"));
        // Exactly 7 fields per the DoD spec.
        assert_eq!(obj.len(), 7, "expected 7 fields, got {obj:?}");
    }

    // --- GetCredentialResponse serialization ---

    #[test]
    fn test_get_response_serialize_includes_value() {
        let resp = GetCredentialResponse {
            id: "cred_01H".to_string(),
            name: "github_token".to_string(),
            kind: "api_token".to_string(),
            value: "ghp_secret123".to_string(),
            env_var: Some("GITHUB_TOKEN".to_string()),
            mount_path: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"value\":\"ghp_secret123\""), "json: {json}");
        assert!(json.contains("\"id\":\"cred_01H\""), "json: {json}");
        assert!(json.contains("\"name\":\"github_token\""), "json: {json}");
        assert!(json.contains("\"kind\":\"api_token\""), "json: {json}");
        assert!(json.contains("\"env_var\":\"GITHUB_TOKEN\""), "json: {json}");
        assert!(json.contains("\"mount_path\":null"), "json: {json}");
    }

    #[test]
    fn test_get_response_field_names_match_dod() {
        let resp = GetCredentialResponse {
            id: "i".to_string(),
            name: "n".to_string(),
            kind: "k".to_string(),
            value: "v".to_string(),
            env_var: None,
            mount_path: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 6, "expected exactly 6 fields, got {obj:?}");
        for k in ["id", "name", "kind", "value", "env_var", "mount_path"] {
            assert!(obj.contains_key(k), "missing field {k}");
        }
    }

    // --- RotateCredentialRequest deserialization ---

    #[test]
    fn test_rotate_request_deserialize() {
        let json = r#"{"value":"new_secret_456"}"#;
        let req: RotateCredentialRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.value, "new_secret_456");
    }

    #[test]
    fn test_rotate_request_missing_value_fails() {
        let json = r#"{}"#;
        assert!(serde_json::from_str::<RotateCredentialRequest>(json).is_err());
    }

    // --- RotateCredentialResponse serialization ---

    #[test]
    fn test_rotate_response_serialize() {
        let resp = RotateCredentialResponse {
            id: "cred_01H".to_string(),
            name: "github_token".to_string(),
            rotated_at: "2025-03-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":\"cred_01H\""), "json: {json}");
        assert!(json.contains("\"name\":\"github_token\""), "json: {json}");
        assert!(
            json.contains("\"rotated_at\":\"2025-03-01T00:00:00Z\""),
            "json: {json}"
        );
    }

    // --- AgentCredentialResponse serialization ---

    #[test]
    fn test_agent_response_serialize_no_id() {
        // The agent response mirrors GetCredentialResponse but omits `id`.
        let resp = AgentCredentialResponse {
            name: "github_token".to_string(),
            kind: "api_token".to_string(),
            value: "ghp_secret123".to_string(),
            env_var: Some("GITHUB_TOKEN".to_string()),
            mount_path: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("id"), "agent response must not include id");
        assert!(!obj.contains_key("tenant_id"));
        assert_eq!(obj.len(), 5, "expected exactly 5 fields, got {obj:?}");
        assert!(json.contains("\"value\":\"ghp_secret123\""), "json: {json}");
        assert!(json.contains("\"env_var\":\"GITHUB_TOKEN\""), "json: {json}");
    }

    // --- extract_token ---

    #[test]
    fn test_extract_token_valid_bearer() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer stronghold_agent_abc".parse().unwrap(),
        );
        let token = extract_token(&headers).unwrap();
        assert_eq!(token, "stronghold_agent_abc");
    }

    #[test]
    fn test_extract_token_missing_header() {
        let headers = axum::http::HeaderMap::new();
        let err = extract_token(&headers);
        assert!(err.is_err());
        let (code, _) = err.unwrap_err();
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_extract_token_wrong_scheme() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Basic abc".parse().unwrap());
        let err = extract_token(&headers);
        assert!(err.is_err());
        let (code, _) = err.unwrap_err();
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_extract_token_empty_bearer() {
        // "Bearer " with nothing after — auth header parsing should still
        // return an empty token string (downstream verify_agent_token will
        // reject it). This matches the behaviour of routes/agent.rs.
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer ".parse().unwrap());
        let token = extract_token(&headers).unwrap();
        assert_eq!(token, "");
    }
}
