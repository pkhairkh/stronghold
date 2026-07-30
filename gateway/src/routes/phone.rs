//! Phone-side endpoints.
//!
//! These endpoints are consumed by the tenant's phone browser (via ntfy
//! deep-links). No custom app — everything runs in the phone's native
//! browser (Safari/Chrome).
//!
//! Endpoints:
//! - `GET  /setup`                       — One-time enrollment page (HTML)
//! - `POST /phone/ceremony/begin`        — Begin a WebAuthn registration ceremony
//! - `POST /phone/ceremony/finish`       — Finish registration: verify attestation, store credential
//! - `POST /phone/enroll`                — Legacy alias for ceremony/finish (same flow, different response shape)
//! - `GET  /phone/pending`               — SSE stream of pending approval requests
//! - `POST /phone/decide`                — Submit a WebAuthn assertion (approve/deny)
//! - `POST /phone/revoke`                — Revoke an active session
//!
//! U3: All WebAuthn flows now perform **real** cryptographic verification
//! via `crypto::webauthn::verify_attestation` and `verify_assertion`. The
//! legacy "trust the client-supplied public_key" enrollment path has been
//! removed.

use crate::routes::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Sse};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::crypto::webauthn::{
    self, AuthenticatorAttestationResponse, AttestationResult, DEFAULT_RP_ID, DEFAULT_RP_ORIGIN,
};

/// Serve the one-time enrollment page (HTML + JS).
pub async fn setup_page() -> impl IntoResponse {
    Html(include_str!("../../../phone/enroll.html"))
}

// ============================================================================
// Shared: WebAuthn assertion shape
// ============================================================================

/// `AuthenticatorAssertionResponse` as produced by
/// `navigator.credentials.get().response`. Each `ArrayBuffer` field is
/// base64url-encoded (the signature is sometimes standard-base64 from
/// `btoa()` — the flexible decoder in `verify_assertion` handles both).
///
/// This struct is the wire shape consumed by
/// `crypto::webauthn::verify_assertion`. The `POST /phone/decide` route
/// accepts a *flat* `DecideRequest` (per the U3 spec) and constructs a
/// `WebAuthnAssertion` from it before calling `verify_assertion`.
#[derive(Debug, Deserialize, Clone)]
pub struct WebAuthnAssertion {
    pub credential_id: String,
    pub authenticator_data: String,
    pub client_data_json: String,
    pub signature: String,
}

// ============================================================================
// POST /phone/enroll — verify an attestation and store the credential.
// ============================================================================

/// Body for `POST /phone/enroll` (U3 rewrite). The `challenge_id` was
/// returned by `POST /phone/ceremony/begin`; the gateway uses it to look
/// up the stored challenge and bind the new credential to the correct
/// tenant. `credential_id` is a *hint* — the gateway independently
/// extracts the credential_id from the verified attestation and rejects
/// the request if they don't match (anti-confused-deputy).
#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    pub challenge_id: String,
    pub credential_id: String,      // base64url
    pub attestation_object: String, // base64url
    pub client_data_json: String,   // base64url
    #[serde(default)]
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
    tracing::info!(
        challenge_id = %req.challenge_id,
        name = %req.name,
        "Credential enrollment via verified attestation"
    );

    let (tenant_id, credential_id) = finish_ceremony(
        &state,
        &req.challenge_id,
        &req.credential_id,
        &req.attestation_object,
        &req.client_data_json,
        &req.name,
    )?;

    Ok(Json(EnrollResponse {
        tenant_id,
        credential_id,
        verified: true,
    }))
}

// ============================================================================
// POST /phone/ceremony/finish — same flow, different response shape.
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CeremonyFinishRequest {
    pub challenge_id: String,
    pub credential_id: String,      // base64url
    pub attestation_object: String, // base64url
    pub client_data_json: String,   // base64url
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CeremonyFinishResponse {
    pub credential_id: String,
    pub tenant_id: String,
}

pub async fn ceremony_finish(
    State(state): State<AppState>,
    Json(req): Json<CeremonyFinishRequest>,
) -> Result<Json<CeremonyFinishResponse>, (StatusCode, String)> {
    tracing::info!(
        challenge_id = %req.challenge_id,
        name = %req.name,
        "Ceremony finish: verifying attestation"
    );

    let (tenant_id, credential_id) = finish_ceremony(
        &state,
        &req.challenge_id,
        &req.credential_id,
        &req.attestation_object,
        &req.client_data_json,
        &req.name,
    )?;

    Ok(Json(CeremonyFinishResponse {
        credential_id,
        tenant_id,
    }))
}

/// Shared registration-completion logic for `/phone/enroll` and
/// `/phone/ceremony/finish`:
///
/// 1. Atomically consume the stored challenge (by `challenge_id`) — this
///    also yields the `tenant_id` the challenge was bound to.
/// 2. Build an `AuthenticatorAttestationResponse` and call
///    `verify_attestation` (W3C §7.1): checks `clientDataJSON.type ==
///    "webauthn.create"`, origin, challenge, RP ID hash, UV flag, parses
///    the attested credential data + COSE_Key, and (for `"packed"`)
///    verifies the self-attestation signature.
/// 3. Sanity-check that the extracted `credential_id` matches the
///    client-supplied hint.
/// 4. Insert the credential into the `credentials` table with
///    `verified = 1` and the initial counter from the attestation.
///
/// Returns `(tenant_id, credential_id)` on success. Any failure returns
/// `401 Unauthorized` (bad attestation) or `500` (DB error).
fn finish_ceremony(
    state: &AppState,
    challenge_id: &str,
    credential_id_hint: &str,
    attestation_object_b64: &str,
    client_data_json_b64: &str,
    name: &str,
) -> Result<(String, String), (StatusCode, String)> {
    // 1. Look up + atomically consume the stored challenge.
    let (tenant_id, challenge) =
        match webauthn::take_challenge_by_id(&state.db, challenge_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            Some(v) => v,
            None => {
                tracing::warn!(
                    challenge_id,
                    "enrollment finish: challenge not found or already used"
                );
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "challenge not found or already used".to_string(),
                ));
            }
        };

    // 2. Build the attestation response and verify it.
    let attestation = AuthenticatorAttestationResponse {
        client_data_json: client_data_json_b64.to_string(),
        attestation_object: attestation_object_b64.to_string(),
    };
    let result: AttestationResult = match webauthn::verify_attestation(
        &attestation,
        &challenge,
        DEFAULT_RP_ORIGIN,
        DEFAULT_RP_ID,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                tenant = %tenant_id,
                challenge_id,
                error = %e,
                "enrollment finish: attestation verification failed"
            );
            return Err((
                StatusCode::UNAUTHORIZED,
                "attestation verification failed".to_string(),
            ));
        }
    };

    // 3. The credential_id extracted from the attestation MUST match the
    //    client-supplied hint. (They should be identical — this guards
    //    against a confused-deputy client sending a different credential_id
    //    in the JSON body vs. the one actually inside the attestationObject.)
    if result.credential_id != credential_id_hint {
        tracing::warn!(
            tenant = %tenant_id,
            challenge_id,
            expected = credential_id_hint,
            actual = %result.credential_id,
            "enrollment finish: credential_id mismatch"
        );
        return Err((
            StatusCode::BAD_REQUEST,
            "credential_id does not match attestation".to_string(),
        ));
    }

    // 4. Store the credential.
    let cred_row_id = crate::tenants::auth::store_credential_from_attestation(
        &state.db,
        &tenant_id,
        &result,
        name,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        cred_row = %cred_row_id,
        credential_id = %result.credential_id,
        fmt = %result.fmt,
        "Credential enrolled via verified attestation"
    );

    Ok((tenant_id, result.credential_id))
}

// ============================================================================
// GET /phone/pending — SSE stream of pending approval requests.
// ============================================================================

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

// ============================================================================
// POST /phone/decide — verify an assertion and approve/deny a session.
// ============================================================================

/// Body for `POST /phone/decide` (U3 rewrite). Flat shape (no nested
/// `WebAuthnAssertion` object) per the U3 spec. The phone authenticates
/// via `Authorization: Bearer <phone_token>`; the phone_token resolves to
/// the tenant_id used for credential lookup.
#[derive(Debug, Deserialize)]
pub struct DecideRequest {
    pub session_id: String,
    pub decision: String, // "approve" or "deny"
    pub credential_id: String,      // base64url
    pub authenticator_data: String, // base64url
    pub client_data_json: String,   // base64url
    pub signature: String,          // base64url (or standard base64)
}

#[derive(Debug, Serialize)]
pub struct DecideResponse {
    pub session_id: String,
    pub decision: String,
}

pub async fn decide(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<DecideRequest>,
) -> Result<Json<DecideResponse>, (StatusCode, String)> {
    // 1. Authenticate the phone via Bearer token → tenant_id.
    let phone_token = extract_phone_token(&headers)?;
    let tenant_id = crate::tenants::auth::verify_phone_token(&state.db, &phone_token)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    tracing::info!(
        tenant = %tenant_id,
        session = %req.session_id,
        decision = %req.decision,
        "Phone decision received"
    );

    // 2. Look up + atomically consume the challenge bound to this pending
    //    session. `verify_assertion` derives the expected challenge
    //    deterministically from `session_id` (via `generate_challenge("",
    //    session_id, "")`); the `phone_challenges` row keyed by `session_id`
    //    holds that same value and enforces single-use (replay prevention
    //    at the route layer, in addition to the W3C counter check inside
    //    `verify_assertion`).
    let _stored_challenge =
        match webauthn::take_challenge(&state.db, &req.session_id, &tenant_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            Some(c) => c,
            None => {
                tracing::warn!(
                    tenant = %tenant_id,
                    session = %req.session_id,
                    "decide: session challenge not found or already used"
                );
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "session challenge not found or already used".to_string(),
                ));
            }
        };

    // 3. Build the WebAuthn assertion from the flat request fields.
    let assertion = WebAuthnAssertion {
        credential_id: req.credential_id.clone(),
        authenticator_data: req.authenticator_data.clone(),
        client_data_json: req.client_data_json.clone(),
        signature: req.signature.clone(),
    };

    // 4. Verify the assertion. `verify_assertion` performs:
    //    - client_data validation (type == "webauthn.get", origin, challenge)
    //    - authenticator_data parse + UV flag check
    //    - RP ID hash check
    //    - credential lookup (load_credential_public_key + counter)
    //    - W3C §6.1 step 18 counter replay protection
    //    - ECDSA P-256 signature verification
    //    - W3C §6.1 step 19 counter advance on success
    let verified =
        webauthn::verify_assertion(&state.db, &tenant_id, &assertion, &req.session_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !verified {
        tracing::warn!(
            tenant = %tenant_id,
            session = %req.session_id,
            credential = %req.credential_id,
            "decide: WebAuthn assertion verification failed"
        );
        return Err((
            StatusCode::UNAUTHORIZED,
            "WebAuthn assertion verification failed".to_string(),
        ));
    }

    // 5. Apply the decision.
    match req.decision.as_str() {
        "approve" => crate::sessions::manager::approve_session(&state.db, &req.session_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        "deny" => crate::sessions::manager::deny_session(&state.db, &req.session_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid decision: {} (expected \"approve\" or \"deny\")", other),
            ));
        }
    }

    tracing::info!(
        tenant = %tenant_id,
        session = %req.session_id,
        decision = %req.decision,
        "Phone decision applied after WebAuthn verification"
    );

    Ok(Json(DecideResponse {
        session_id: req.session_id,
        decision: req.decision,
    }))
}

// ============================================================================
// POST /phone/revoke — instant session kill.
// ============================================================================

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
// U1: WebAuthn ceremony generation (POST /phone/ceremony/begin)
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CeremonyBeginParams {
    pub tenant: String,
}

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

// ============================================================================
// Internal helpers
// ============================================================================

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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::webauthn::{
        generate_challenge, generate_ceremony_options, store_challenge, take_challenge_by_id,
        verify_assertion, verify_attestation,
    };
    use crate::db::init_memory_pool;
    use crate::routes::agent::OrderRequest;
    use crate::sessions::manager::{approve_session, create_pending};
    use crate::tenants::auth::store_credential_from_attestation;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use p256::ecdsa::{signature::Signer, DerSignature, SigningKey};
    use rand::rngs::OsRng;
    use rusqlite::params;
    use sha2::{Digest, Sha256};

    // Minimal CBOR builders (mirrors of the private helpers in webauthn.rs).
    fn build_cose_key(x: &[u8], y: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0xa5);
        out.push(0x01); out.push(0x02);
        out.push(0x03); out.push(0x26);
        out.push(0x20); out.push(0x01);
        out.push(0x21); out.push(0x58); out.push(32); out.extend_from_slice(x);
        out.push(0x22); out.push(0x58); out.push(32); out.extend_from_slice(y);
        out
    }

    fn build_auth_data_with_attested_cred(
        rp_id: &str,
        sign_count: u32,
        aaguid: &[u8; 16],
        credential_id: &[u8],
        cose_key_cbor: &[u8],
    ) -> Vec<u8> {
        let rp_id_hash = Sha256::digest(rp_id.as_bytes());
        let mut out = Vec::new();
        out.extend_from_slice(&rp_id_hash);
        out.push(0x45);
        out.extend_from_slice(&sign_count.to_be_bytes());
        out.extend_from_slice(aaguid);
        out.extend_from_slice(&(credential_id.len() as u16).to_be_bytes());
        out.extend_from_slice(credential_id);
        out.extend_from_slice(cose_key_cbor);
        out
    }

    fn build_empty_att_stmt() -> Vec<u8> { vec![0xa0] }

    fn build_attestation_object(fmt: &str, auth_data: &[u8], att_stmt_cbor: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0xa3);
        out.push(0x63); out.extend_from_slice(b"fmt");
        out.push(0x60 + fmt.len() as u8); out.extend_from_slice(fmt.as_bytes());
        out.push(0x68); out.extend_from_slice(b"authData");
        out.push(0x58); out.push(auth_data.len() as u8);
        out.extend_from_slice(auth_data);
        out.push(0x67); out.extend_from_slice(b"attStmt");
        out.extend_from_slice(att_stmt_cbor);
        out
    }

    fn setup_tenant(pool: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>, tenant_id: &str) {
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES (?1, 'T', datetime('now'), 'x', 0)",
            params![tenant_id],
        )
        .unwrap();
    }

    /// End-to-end U3 test: generate ceremony → fake attestation → verify →
    /// store credential → create session → fake assertion → verify → approve.
    #[test]
    fn test_full_webauthn_enrollment_and_decide() {
        let pool = init_memory_pool().unwrap();
        setup_tenant(&pool, "t-enroll");

        // Step 1: Begin ceremony (generate options + store challenge).
        let (_options, challenge) = generate_ceremony_options(
            "t-enroll", DEFAULT_RP_ID, "Stronghold", "t-enroll", "T",
        );
        let challenge_id = "cerem-01HXYZ";
        store_challenge(&pool, challenge_id, "t-enroll", &challenge).unwrap();

        // Step 2: Build a fake attestation with a fresh P-256 key.
        let mut rng = OsRng;
        let signing_key = SigningKey::random(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let sec1 = verifying_key.to_sec1_bytes();
        let cose_key = build_cose_key(&sec1[1..33], &sec1[33..65]);
        let raw_cred_id = b"cred-id-1234567890";
        let auth_data = build_auth_data_with_attested_cred(
            DEFAULT_RP_ID, 1, &[0u8; 16], raw_cred_id, &cose_key,
        );
        let att_obj = build_attestation_object("none", &auth_data, &build_empty_att_stmt());
        let att_obj_b64 = URL_SAFE_NO_PAD.encode(&att_obj);

        let challenge_b64 = URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 = URL_SAFE_NO_PAD.encode(&client_data_bytes);

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        // Step 3: Verify the attestation.
        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID)
            .expect("attestation should verify");
        assert_eq!(result.fmt, "none");
        assert_eq!(result.sign_count, 1);
        let decoded_cred_id = URL_SAFE_NO_PAD.decode(&result.credential_id).unwrap();
        assert_eq!(decoded_cred_id, raw_cred_id);
        let decoded_pk = URL_SAFE_NO_PAD.decode(&result.public_key).unwrap();
        assert_eq!(&decoded_pk[..], &sec1[..]);

        // Step 4: Store the verified credential.
        let cred_row_id = store_credential_from_attestation(&pool, "t-enroll", &result, "Test")
            .expect("credential should be stored");
        assert!(!cred_row_id.is_empty());

        // Verify the credential row has verified=1, counter=1.
        {
            let conn = pool.get().unwrap();
            let (pk, verified, counter): (String, i64, i64) = conn.query_row(
                "SELECT public_key, verified, counter FROM credentials
                 WHERE credential_id = ?1 AND tenant_id = ?2",
                params![&result.credential_id, "t-enroll"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            assert_eq!(pk, result.public_key);
            assert_eq!(verified, 1);
            assert_eq!(counter, 1);
        }

        // Step 5: The ceremony challenge is single-use — first take returns
        //        Some(challenge), second take returns None. (verify_attestation
        //        itself does not consume the challenge; only the route handler does.)
        let taken1 = take_challenge_by_id(&pool, challenge_id).unwrap();
        assert!(taken1.is_some(), "ceremony challenge should be available on first take");
        let taken2 = take_challenge_by_id(&pool, challenge_id).unwrap();
        assert!(taken2.is_none(), "ceremony challenge must be consumed after first take");

        // Step 6: Create a pending session + store its challenge.
        let order = OrderRequest {
            image: "test-image".to_string(),
            ttl_secs: 3600,
            reason: "test".to_string(),
            compute: Default::default(),
            ephemeral_volumes: vec![],
        };
        let session_id = create_pending(&pool, "t-enroll", &order).unwrap();
        assert!(session_id.starts_with("sess_"));

        // The decide route looks up the challenge by session_id and consumes
        // it. The stored value MUST equal generate_challenge("", session_id,
        // "") because that is what verify_assertion derives internally.
        let session_challenge = generate_challenge("", &session_id, "");
        store_challenge(&pool, &session_id, "t-enroll", &session_challenge).unwrap();

        // Step 7: Build a fake assertion (signed with the same key).
        let challenge_b64 = URL_SAFE_NO_PAD.encode(&session_challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 = URL_SAFE_NO_PAD.encode(&client_data_bytes);

        let rp_id_hash = {
            let mut hasher = Sha256::new();
            hasher.update(DEFAULT_RP_ID.as_bytes());
            let r = hasher.finalize();
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&r);
            arr
        };
        // Stored counter is 1, so asserted must be strictly greater (2).
        let mut auth_bytes = Vec::new();
        auth_bytes.extend_from_slice(&rp_id_hash);
        auth_bytes.push(0x05);
        auth_bytes.extend_from_slice(&2u32.to_be_bytes());
        let auth_data_b64 = URL_SAFE_NO_PAD.encode(&auth_bytes);

        let client_data_hash = Sha256::digest(&client_data_bytes);
        let mut signed_message = Vec::with_capacity(auth_bytes.len() + client_data_hash.len());
        signed_message.extend_from_slice(&auth_bytes);
        signed_message.extend_from_slice(&client_data_hash);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let signature_b64 = URL_SAFE_NO_PAD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: result.credential_id.clone(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        // Step 8: Verify the assertion.
        let verified = verify_assertion(&pool, "t-enroll", &assertion, &session_id).unwrap();
        assert!(verified, "assertion must verify against the enrolled credential");

        // The stored counter was advanced to 2.
        {
            let conn = pool.get().unwrap();
            let counter: i64 = conn.query_row(
                "SELECT counter FROM credentials WHERE credential_id = ?1",
                params![&result.credential_id],
                |row| row.get(0),
            ).unwrap();
            assert_eq!(counter, 2);
        }

        // Step 9: Approve the session.
        approve_session(&pool, &session_id).unwrap();

        // Step 10: Check session is approved.
        let conn = pool.get().unwrap();
        let status: String = conn.query_row(
            "SELECT status FROM pending_sessions WHERE id = ?1",
            params![&session_id],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(status, "approved");
    }

    /// An invalid attestation (wrong origin) must be rejected by verify_attestation.
    #[test]
    fn test_verify_attestation_rejects_wrong_origin_e2e() {
        let pool = init_memory_pool().unwrap();
        setup_tenant(&pool, "t-enroll2");

        let (_options, challenge) = generate_ceremony_options(
            "t-enroll2", DEFAULT_RP_ID, "Stronghold", "u", "U",
        );

        let mut rng = OsRng;
        let signing_key = SigningKey::random(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let sec1 = verifying_key.to_sec1_bytes();
        let cose_key = build_cose_key(&sec1[1..33], &sec1[33..65]);
        let auth_data = build_auth_data_with_attested_cred(
            DEFAULT_RP_ID, 1, &[0u8; 16], b"cred-id-bad-origin", &cose_key,
        );
        let att_obj = build_attestation_object("none", &auth_data, &build_empty_att_stmt());
        let att_obj_b64 = URL_SAFE_NO_PAD.encode(&att_obj);

        let challenge_b64 = URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge_b64,
            "origin": "https://evil.example.com",
        });
        let client_data_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&client_data).unwrap());

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID);
        assert!(result.is_err(), "wrong origin must be rejected");
    }

    /// An invalid assertion (tampered signature) must be rejected by verify_assertion.
    #[test]
    fn test_verify_assertion_rejects_tampered_e2e() {
        let pool = init_memory_pool().unwrap();
        setup_tenant(&pool, "t-enroll3");

        let mut rng = OsRng;
        let signing_key = SigningKey::random(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let sec1 = verifying_key.to_sec1_bytes();
        let public_key_b64 = URL_SAFE_NO_PAD.encode(&sec1);
        let credential_id = "cred-tampered";
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "INSERT INTO credentials
                 (id, tenant_id, credential_id, public_key, aaguid, transports, name,
                  verified, counter, created_at)
                 VALUES ('c1', 't-enroll3', ?1, ?2, '', '', 'T', 1, 0, datetime('now'))",
                params![credential_id, public_key_b64],
            ).unwrap();
        }

        let session_id = "sess_tampered";
        let session_challenge = generate_challenge("", session_id, "");
        store_challenge(&pool, session_id, "t-enroll3", &session_challenge).unwrap();

        let challenge_b64 = URL_SAFE_NO_PAD.encode(&session_challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 = URL_SAFE_NO_PAD.encode(&client_data_bytes);

        let rp_id_hash = Sha256::digest(DEFAULT_RP_ID.as_bytes());
        let mut auth_bytes = Vec::new();
        auth_bytes.extend_from_slice(&rp_id_hash);
        auth_bytes.push(0x05);
        auth_bytes.extend_from_slice(&1u32.to_be_bytes());

        let client_data_hash = Sha256::digest(&client_data_bytes);
        let mut signed_message = Vec::new();
        signed_message.extend_from_slice(&auth_bytes);
        signed_message.extend_from_slice(&client_data_hash);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let mut sig_bytes = signature.as_bytes().to_vec();
        let mid = sig_bytes.len() / 2;
        sig_bytes[mid] ^= 0x01;
        let signature_b64 = URL_SAFE_NO_PAD.encode(&sig_bytes);

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: URL_SAFE_NO_PAD.encode(&auth_bytes),
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let verified = verify_assertion(&pool, "t-enroll3", &assertion, session_id).unwrap();
        assert!(!verified, "tampered signature must be rejected");
    }
}
