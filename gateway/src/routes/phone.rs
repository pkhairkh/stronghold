//! Phone-side endpoints.
//!
//! These endpoints are consumed by the tenant's phone browser (via ntfy
//! deep-links). No custom app — everything runs in the phone's native
//! browser (Safari/Chrome).
//!
//! Endpoints:
//! - `GET  /setup` — One-time enrollment page (HTML)
//! - `POST /phone/enroll` — Enroll a new WebAuthn credential
//! - `GET  /phone/pending` — SSE stream of pending approval requests
//! - `POST /phone/decide` — Submit a WebAuthn assertion (approve/deny)
//! - `POST /phone/revoke` — Revoke an active session

use crate::routes::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Sse};
use axum::Json;
use serde::{Deserialize, Serialize};

/// Serve the one-time enrollment page (HTML + JS).
pub async fn setup_page() -> impl IntoResponse {
    Html(include_str!("../../../phone/enroll.html"))
}

/// Enroll a new WebAuthn credential.
#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    pub setup_password: String,
    pub credential_id: String,
    pub public_key: String,
    pub aaguid: String,
    pub transports: Vec<String>,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct EnrollResponse {
    pub tenant_id: String,
    pub credential_id: String,
    pub verified: bool,
}

pub async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, (StatusCode, String)> {
    tracing::info!("Credential enrollment: name={}", req.name);

    // Verify setup password
    crate::tenants::auth::verify_setup_password(&state.db, &req.setup_password)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    // Store credential
    let tenant_id = crate::tenants::auth::enroll_credential(&state.db, &req)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(EnrollResponse {
        tenant_id,
        credential_id: req.credential_id,
        verified: true,
    }))
}

/// SSE stream of pending approval requests for a given tenant.
///
/// The phone opens this as a persistent connection. When a new ORDER
/// or EXTEND comes in, the gateway pushes an `approval_request` event
/// over this stream.
pub async fn pending_sse(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<
    Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    (StatusCode, String),
> {
    let phone_token = extract_phone_token(&headers)?;
    let tenant_id = crate::tenants::auth::verify_phone_token(&state.db, &phone_token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    tracing::info!(tenant = %tenant_id, "Phone SSE connected");

    let stream = crate::sessions::manager::pending_approval_stream(&state.db, &tenant_id);
    Ok(Sse::new(stream))
}

/// Submit a WebAuthn assertion to approve or deny a session.
#[derive(Debug, Deserialize)]
pub struct DecideRequest {
    pub request_id: String,
    pub decision: String, // "approve" or "deny"
    pub assertion: WebAuthnAssertion,
}

#[derive(Debug, Deserialize)]
pub struct WebAuthnAssertion {
    pub credential_id: String,
    pub authenticator_data: String,
    pub client_data_json: String,
    pub signature: String,
}

pub async fn decide(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<DecideRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let phone_token = extract_phone_token(&headers)?;
    let tenant_id = crate::tenants::auth::verify_phone_token(&state.db, &phone_token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        request = %req.request_id,
        decision = %req.decision,
        "Phone decision received"
    );

    // Verify WebAuthn assertion
    let verified = crate::crypto::webauthn::verify_assertion(
        &state.db,
        &tenant_id,
        &req.assertion,
        &req.request_id,
    )
    .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    if !verified {
        return Err((
            StatusCode::UNAUTHORIZED,
            "WebAuthn assertion verification failed".to_string(),
        ));
    }

    match req.decision.as_str() {
        "approve" => {
            crate::sessions::manager::approve_session(&state.db, &req.request_id)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            Ok(StatusCode::OK)
        }
        "deny" => {
            crate::sessions::manager::deny_session(&state.db, &req.request_id)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            Ok(StatusCode::OK)
        }
        _ => Err((StatusCode::BAD_REQUEST, "Invalid decision".to_string())),
    }
}

/// Revoke an active session (instant kill).
#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub machine_id: String,
}

pub async fn revoke(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RevokeRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let phone_token = extract_phone_token(&headers)?;
    let tenant_id = crate::tenants::auth::verify_phone_token(&state.db, &phone_token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        machine = %req.machine_id,
        "Phone REVOKE received"
    );

    crate::sessions::manager::revoke_session(&state, &tenant_id, &req.machine_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::OK)
}


// ============================================================================
// U1: WebAuthn ceremony generation
// ============================================================================

/// Query parameters for `POST /phone/ceremony/begin`.
#[derive(Debug, Deserialize)]
pub struct CeremonyBeginParams {
    pub tenant: String,
}

/// Response wrapper for the ceremony-begin endpoint. Includes the
/// `PublicKeyCredentialCreationOptions` plus the `challenge_id` the
/// client must echo back to `/phone/ceremony/finish` (added in a later
/// wave) so the gateway can look up the stored challenge.
#[derive(Debug, Serialize)]
pub struct CeremonyBeginResponse {
    #[serde(flatten)]
    pub options: crate::crypto::webauthn::PublicKeyCredentialCreationOptions,
    pub challenge_id: String,
}

/// `POST /phone/ceremony/begin?tenant=<id>` — begin a WebAuthn
/// registration ceremony. Generates fresh ceremony options (random
/// 32-byte challenge, ES256+RS256 params, platform authenticator,
/// UV required), stores the challenge in `phone_challenges`, and
/// returns the options + challenge_id as JSON.
pub async fn ceremony_begin(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<CeremonyBeginParams>,
) -> Result<Json<CeremonyBeginResponse>, (StatusCode, String)> {
    let tenant_id = params.tenant;
    tracing::info!(tenant = %tenant_id, "Beginning WebAuthn registration ceremony");

    let (options, challenge) = crate::crypto::webauthn::generate_ceremony_options(
        &tenant_id,
        crate::crypto::webauthn::DEFAULT_RP_ID,
        "Stronghold",
        &tenant_id,
        &tenant_id,
    );

    let challenge_id = ulid::Ulid::new().to_string();
    crate::crypto::webauthn::store_challenge(&state.db, &challenge_id, &tenant_id, &challenge)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(CeremonyBeginResponse {
        options,
        challenge_id,
    }))
}


fn extract_phone_token(headers: &axum::http::HeaderMap) -> Result<String, (StatusCode, String)> {
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
