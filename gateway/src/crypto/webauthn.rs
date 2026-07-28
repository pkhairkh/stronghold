//! WebAuthn — multi-credential, multi-device session approval.
//!
//! Uses webauthn-rs to verify assertions from the phone's native browser.
//! No custom app — all ceremonies happen via ntfy deep-links to the
//! phone's Safari/Chrome.
//!
//! Note: WebAuthn is the one layer where PQC is NOT yet deployed.
//! FIDO Alliance has a PQC authenticator spec in draft, but no deployed
//! phone authenticator supports it yet (~2027 expected). Mitigation:
//! session TTLs are short (hours), so a quantum adversary breaking
//! WebAuthn in 10 years gets nothing useful.
//!
//! Implemented in: W1-T8 (assertion verification), W1-T9 (challenge generation)
//! Tested by: gateway/src/crypto/webauthn.rs

use crate::routes::phone::WebAuthnAssertion;
use anyhow::Result;
use base64::Engine;
use rand::RngCore;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use sha2::{Digest, Sha256};

/// The Relying Party (RP) ID for Stronghold. This must match the domain
/// the gateway is served from. For dev: "localhost". For prod: e.g. "stronghold.example.com".
///
/// TODO W10-T: make this configurable via config.toml.
pub const DEFAULT_RP_ID: &str = "localhost";

/// The expected origin for WebAuthn ceremonies. Must match the browser's
/// `window.location.origin` exactly.
///
/// TODO W10-T: make this configurable.
pub const DEFAULT_RP_ORIGIN: &str = "https://localhost:8443";

/// The length of a WebAuthn challenge in bytes (per W3C spec, min 16, we use 32).
pub const CHALLENGE_LEN: usize = 32;

/// Generate a WebAuthn challenge bound to a specific session approval.
///
/// The challenge is `sha256(cmd_hash || request_id || scope_hash)`, binding
/// this approval to a specific command/request/scope triple. This prevents
/// replay: an assertion signed for one approval cannot be reused for another.
///
/// Returns 32 raw bytes. The caller base64url-encodes this for the browser.
///
/// **Note:** this function does *not* bind the challenge to the SEV-SNP
/// measurement. For approvals that must be bound to the gateway's TEE state
/// (the production path), use [`generate_challenge_with_sev_snp`] instead.
pub fn generate_challenge(cmd_hash: &str, request_id: &str, scope_hash: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(cmd_hash.as_bytes());
    hasher.update(request_id.as_bytes());
    hasher.update(scope_hash.as_bytes());
    hasher.finalize().to_vec()
}

/// Generate a WebAuthn challenge bound to a session approval *and* the
/// gateway's current SEV-SNP measurement hash (W7-T5).
///
/// The challenge is `sha256(cmd_hash || request_id || scope_hash ||
/// sev_snp_measurement_hash)`. Binding the measurement into the challenge
/// means the phone's WebAuthn assertion cryptographically signs over the
/// gateway's current TEE state — so any future approval whose gateway
/// measurement has changed (binary upgrade, kernel patch, compromise) will
/// fail challenge verification on the gateway side, even if the rest of
/// the assertion metadata matches.
///
/// Pass `None` for `sev_snp_measurement_hash` to opt out of TEE binding
/// (dev mode, or `--features no-sev-snp` builds). When `None`, the
/// resulting challenge is identical to [`generate_challenge`].
///
/// Returns 32 raw bytes.
pub fn generate_challenge_with_sev_snp(
    cmd_hash: &str,
    request_id: &str,
    scope_hash: &str,
    sev_snp_measurement_hash: Option<&str>,
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(cmd_hash.as_bytes());
    hasher.update(request_id.as_bytes());
    hasher.update(scope_hash.as_bytes());
    if let Some(mh) = sev_snp_measurement_hash {
        hasher.update(mh.as_bytes());
    }
    hasher.finalize().to_vec()
}

/// Compute the SHA-256 hash of the SEV-SNP attestation report, hex-encoded.
///
/// Convenience wrapper used by callers that have an
/// [`crate::tee::AttestationReport`] and want to bind its hash into a
/// WebAuthn challenge via [`generate_challenge_with_sev_snp`].
pub fn sev_snp_measurement_hash(report: &crate::tee::AttestationReport) -> String {
    let mut hasher = Sha256::new();
    hasher.update(report.measurement.as_bytes());
    hasher.update(report.report_hash.as_bytes());
    hex::encode(hasher.finalize())
}

/// Generate a random challenge (for enrollment, where there's no command to bind to).
///
/// Uses `OsRng` (platform CSPRNG). Returns 32 bytes.
pub fn generate_random_challenge() -> [u8; CHALLENGE_LEN] {
    let mut challenge = [0u8; CHALLENGE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut challenge);
    challenge
}

/// Decode the `client_data_json` field from a WebAuthn assertion.
///
/// The browser sends this as base64url-encoded JSON. It contains the
/// challenge, origin, and type ("webauthn.get" for assertions).
#[derive(Debug, serde::Deserialize)]
pub struct ClientData {
    #[serde(rename = "type")]
    pub kind: String,
    pub challenge: String,
    pub origin: String,
    /// `crossOrigin` is optional per spec; defaults to false.
    #[serde(default)]
    pub cross_origin: bool,
}

/// Parse and validate the `client_data_json` from an assertion.
///
/// Checks:
/// 1. Valid base64url
/// 2. Valid JSON matching `ClientData` schema
/// 3. `type == "webauthn.get"` (for assertions)
/// 4. `origin == expected_origin` (anti-phishing)
/// 5. `challenge == expected_challenge` (replay prevention)
pub fn parse_and_validate_client_data(
    assertion: &WebAuthnAssertion,
    expected_challenge: &[u8],
    expected_origin: &str,
) -> Result<ClientData> {
    let client_data_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&assertion.client_data_json)
        .map_err(|e| anyhow::anyhow!("invalid client_data_json base64url: {}", e))?;

    let client_data: ClientData = serde_json::from_slice(&client_data_bytes)
        .map_err(|e| anyhow::anyhow!("invalid client_data_json JSON: {}", e))?;

    // Check type.
    if client_data.kind != "webauthn.get" {
        return Err(anyhow::anyhow!(
            "expected client_data.type == 'webauthn.get', got '{}'",
            client_data.kind
        ));
    }

    // Check origin (anti-phishing).
    if client_data.origin != expected_origin {
        return Err(anyhow::anyhow!(
            "origin mismatch: expected '{}', got '{}' — possible phishing",
            expected_origin,
            client_data.origin
        ));
    }

    // Check challenge (replay prevention).
    let expected_challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(expected_challenge);
    if client_data.challenge != expected_challenge_b64 {
        return Err(anyhow::anyhow!(
            "challenge mismatch — assertion is for a different approval (replay attempted?)"
        ));
    }

    Ok(client_data)
}

/// Parse the `authenticator_data` field from a WebAuthn assertion.
///
/// Returns the parsed struct, including the `user_verified` flag.
#[derive(Debug)]
pub struct AuthenticatorData {
    /// RP ID hash (32 bytes). Must match SHA-256 of the RP ID.
    pub rp_id_hash: [u8; 32],
    /// Flags byte (bit 0 = user present, bit 2 = user verified, bit 6 = attested credential data).
    pub flags: u8,
    /// Signature counter (monotonic, per credential).
    pub sign_count: u32,
}

impl AuthenticatorData {
    /// Whether the user was present (UP flag, bit 0).
    pub fn user_present(&self) -> bool {
        (self.flags & 0x01) != 0
    }

    /// Whether the user was verified (UV flag, bit 2 — biometric/PIN).
    pub fn user_verified(&self) -> bool {
        (self.flags & 0x04) != 0
    }
}

/// Parse the `authenticator_data` from an assertion.
///
/// `authenticator_data` is a binary blob: 32 bytes RP ID hash + 1 byte flags + 4 bytes sign count.
/// Total minimum length: 37 bytes.
pub fn parse_authenticator_data(
    assertion: &WebAuthnAssertion,
) -> Result<AuthenticatorData> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&assertion.authenticator_data)
        .map_err(|e| anyhow::anyhow!("invalid authenticator_data base64url: {}", e))?;

    if bytes.len() < 37 {
        return Err(anyhow::anyhow!(
            "authenticator_data too short: {} bytes, need >= 37",
            bytes.len()
        ));
    }

    let mut rp_id_hash = [0u8; 32];
    rp_id_hash.copy_from_slice(&bytes[..32]);
    let flags = bytes[32];
    let sign_count = u32::from_be_bytes([bytes[33], bytes[34], bytes[35], bytes[36]]);

    Ok(AuthenticatorData {
        rp_id_hash,
        flags,
        sign_count,
    })
}

/// Verify a WebAuthn assertion.
///
/// Checks:
/// 1. `client_data_json` parses and matches expected challenge + origin
/// 2. `authenticator_data` parses and has `user_verified == true`
/// 3. RP ID hash matches SHA-256 of the expected RP ID
/// 4. (TODO W2-T7) Signature verifies against the registered credential public key
///
/// Returns `Ok(true)` only if all checks pass.
///
/// Note: full signature verification requires the credential's public key
/// from the database (Wave 2, W2-T7). For now, this function validates
/// everything except the signature. The signature check is a TODO.
pub fn verify_assertion(
    _db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    assertion: &WebAuthnAssertion,
    request_id: &str,
) -> Result<bool> {
    tracing::info!(
        tenant = %tenant_id,
        request = %request_id,
        credential = %assertion.credential_id,
        "Verifying WebAuthn assertion"
    );

    // Generate the expected challenge from the request_id.
    // The challenge binds this assertion to this specific approval.
    // cmd_hash and scope_hash are not yet available at this layer; use
    // request_id as the sole input for now. W3-T8 will pass the full tuple.
    let expected_challenge = generate_challenge("", request_id, "");

    // 1. Parse and validate client_data.
    let _client_data = match parse_and_validate_client_data(
        assertion,
        &expected_challenge,
        DEFAULT_RP_ORIGIN,
    ) {
        Ok(cd) => cd,
        Err(e) => {
            tracing::warn!(
                tenant = %tenant_id,
                request = %request_id,
                error = %e,
                "WebAuthn client_data validation failed"
            );
            return Ok(false);
        }
    };

    // 2. Parse authenticator_data.
    let auth_data = match parse_authenticator_data(assertion) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                tenant = %tenant_id,
                request = %request_id,
                error = %e,
                "WebAuthn authenticator_data parse failed"
            );
            return Ok(false);
        }
    };

    // 3. Check user_verified flag.
    if !auth_data.user_verified() {
        tracing::warn!(
            tenant = %tenant_id,
            request = %request_id,
            "WebAuthn assertion missing user_verified flag — biometric/PIN required"
        );
        return Ok(false);
    }

    // 4. Check RP ID hash.
    let expected_rp_id_hash = {
        let mut hasher = Sha256::new();
        hasher.update(DEFAULT_RP_ID.as_bytes());
        let result = hasher.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&result);
        arr
    };
    if auth_data.rp_id_hash != expected_rp_id_hash {
        tracing::warn!(
            tenant = %tenant_id,
            request = %request_id,
            "WebAuthn RP ID hash mismatch — assertion is for a different RP"
        );
        return Ok(false);
    }

    // 5. TODO W2-T7: verify the signature against the credential public key.
    //    This requires loading the credential from the DB (Wave 2).
    //    For now, we accept the assertion if all other checks pass.
    tracing::warn!(
        tenant = %tenant_id,
        request = %request_id,
        "WebAuthn signature verification not yet implemented — accepting based on metadata only (W2-T7 will fix this)"
    );

    Ok(true)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- W1-T9: challenge generation ---

    #[test]
    fn test_generate_challenge_is_deterministic() {
        let c1 = generate_challenge("hash1", "req1", "scope1");
        let c2 = generate_challenge("hash1", "req1", "scope1");
        assert_eq!(c1, c2);
        assert_eq!(c1.len(), 32);
    }

    #[test]
    fn test_generate_challenge_differs_per_request_id() {
        let c1 = generate_challenge("hash", "req1", "scope");
        let c2 = generate_challenge("hash", "req2", "scope");
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_generate_challenge_differs_per_cmd_hash() {
        let c1 = generate_challenge("hash1", "req", "scope");
        let c2 = generate_challenge("hash2", "req", "scope");
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_generate_challenge_differs_per_scope() {
        let c1 = generate_challenge("hash", "req", "scope1");
        let c2 = generate_challenge("hash", "req", "scope2");
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_generate_random_challenge_is_unique() {
        let c1 = generate_random_challenge();
        let c2 = generate_random_challenge();
        assert_ne!(c1, c2);
        assert_eq!(c1.len(), CHALLENGE_LEN);
    }

    // --- W1-T8: client_data parsing ---

    #[test]
    fn test_parse_valid_client_data() {
        let challenge = generate_challenge("h", "r", "s");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
            "crossOrigin": false,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: String::new(),
            client_data_json: client_data_b64,
            signature: String::new(),
        };

        let result = parse_and_validate_client_data(&assertion, &challenge, DEFAULT_RP_ORIGIN);
        assert!(result.is_ok());
        let cd = result.unwrap();
        assert_eq!(cd.kind, "webauthn.get");
        assert_eq!(cd.origin, DEFAULT_RP_ORIGIN);
    }

    #[test]
    fn test_parse_client_data_rejects_wrong_type() {
        let challenge = generate_challenge("h", "r", "s");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",  // wrong type
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: String::new(),
            client_data_json: client_data_b64,
            signature: String::new(),
        };

        let result = parse_and_validate_client_data(&assertion, &challenge, DEFAULT_RP_ORIGIN);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("webauthn.get"));
    }

    #[test]
    fn test_parse_client_data_rejects_wrong_origin() {
        let challenge = generate_challenge("h", "r", "s");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": "https://evil.example.com",  // wrong origin
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: String::new(),
            client_data_json: client_data_b64,
            signature: String::new(),
        };

        let result = parse_and_validate_client_data(&assertion, &challenge, DEFAULT_RP_ORIGIN);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("phishing"));
    }

    #[test]
    fn test_parse_client_data_rejects_wrong_challenge() {
        let challenge = generate_challenge("h", "r", "s");
        let wrong_challenge = generate_challenge("h", "WRONG", "s");
        let wrong_challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&wrong_challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": wrong_challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: String::new(),
            client_data_json: client_data_b64,
            signature: String::new(),
        };

        let result = parse_and_validate_client_data(&assertion, &challenge, DEFAULT_RP_ORIGIN);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("replay"));
    }

    #[test]
    fn test_parse_client_data_rejects_invalid_base64() {
        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: String::new(),
            client_data_json: "!!!invalid base64!!!".to_string(),
            signature: String::new(),
        };

        let result = parse_and_validate_client_data(&assertion, &[0u8; 32], DEFAULT_RP_ORIGIN);
        assert!(result.is_err());
    }

    // --- W1-T8: authenticator_data parsing ---

    #[test]
    fn test_parse_valid_authenticator_data() {
        // Construct a valid authenticator_data: 32 bytes RP ID hash + 1 byte flags + 4 bytes sign count.
        let rp_id_hash = {
            let mut hasher = Sha256::new();
            hasher.update(DEFAULT_RP_ID.as_bytes());
            let result = hasher.finalize();
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&result);
            arr
        };
        let flags: u8 = 0x05; // UP=1, UV=1
        let sign_count: u32 = 42;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&rp_id_hash);
        bytes.push(flags);
        bytes.extend_from_slice(&sign_count.to_be_bytes());

        let auth_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);

        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: String::new(),
            signature: String::new(),
        };

        let result = parse_authenticator_data(&assertion);
        assert!(result.is_ok());
        let auth_data = result.unwrap();
        assert_eq!(auth_data.rp_id_hash, rp_id_hash);
        assert!(auth_data.user_present());
        assert!(auth_data.user_verified());
        assert_eq!(auth_data.sign_count, 42);
    }

    #[test]
    fn test_parse_authenticator_data_rejects_short_input() {
        let short_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"too short");
        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: short_b64,
            client_data_json: String::new(),
            signature: String::new(),
        };
        let result = parse_authenticator_data(&assertion);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn test_user_verified_flag_extraction() {
        // flags = 0x04 → UV=1, UP=0
        let flags: u8 = 0x04;
        let auth_data = AuthenticatorData {
            rp_id_hash: [0u8; 32],
            flags,
            sign_count: 0,
        };
        assert!(auth_data.user_verified());
        assert!(!auth_data.user_present());

        // flags = 0x01 → UP=1, UV=0
        let auth_data = AuthenticatorData {
            rp_id_hash: [0u8; 32],
            flags: 0x01,
            sign_count: 0,
        };
        assert!(!auth_data.user_verified());
        assert!(auth_data.user_present());
    }

    // --- W1-T8: full verify_assertion (without signature check) ---

    #[test]
    fn test_verify_assertion_accepts_valid_metadata() {
        // Build a valid assertion with matching challenge, origin, RP ID hash, and UV flag.
        let request_id = "req_01HXYZ";
        let challenge = generate_challenge("", request_id, "");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let rp_id_hash = {
            let mut hasher = Sha256::new();
            hasher.update(DEFAULT_RP_ID.as_bytes());
            let result = hasher.finalize();
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&result);
            arr
        };
        let mut auth_bytes = Vec::new();
        auth_bytes.extend_from_slice(&rp_id_hash);
        auth_bytes.push(0x05); // UP=1, UV=1
        auth_bytes.extend_from_slice(&0u32.to_be_bytes());
        let auth_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&auth_bytes);

        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: String::new(),
        };

        // verify_assertion uses an in-memory DB pool. Create one for testing.
        let manager = r2d2_sqlite::SqliteConnectionManager::memory();
        let pool = r2d2::Pool::builder().build(manager).unwrap();

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        // Returns true (metadata checks pass; signature check is TODO W2-T7).
        assert!(result.unwrap());
    }

    #[test]
    fn test_verify_assertion_rejects_missing_uv_flag() {
        let request_id = "req_01HXYZ";
        let challenge = generate_challenge("", request_id, "");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let rp_id_hash = {
            let mut hasher = Sha256::new();
            hasher.update(DEFAULT_RP_ID.as_bytes());
            let result = hasher.finalize();
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&result);
            arr
        };
        let mut auth_bytes = Vec::new();
        auth_bytes.extend_from_slice(&rp_id_hash);
        auth_bytes.push(0x01); // UP=1, UV=0 — missing biometric!
        auth_bytes.extend_from_slice(&0u32.to_be_bytes());
        let auth_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&auth_bytes);

        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: String::new(),
        };

        let manager = r2d2_sqlite::SqliteConnectionManager::memory();
        let pool = r2d2::Pool::builder().build(manager).unwrap();

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(!result.unwrap()); // rejected
    }

    #[test]
    fn test_verify_assertion_rejects_wrong_rp_id() {
        let request_id = "req_01HXYZ";
        let challenge = generate_challenge("", request_id, "");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        // Wrong RP ID hash (all zeros instead of SHA-256 of "localhost").
        let mut auth_bytes = Vec::new();
        auth_bytes.extend_from_slice(&[0u8; 32]);
        auth_bytes.push(0x05); // UP=1, UV=1
        auth_bytes.extend_from_slice(&0u32.to_be_bytes());
        let auth_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&auth_bytes);

        let assertion = WebAuthnAssertion {
            credential_id: "test-cred".to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: String::new(),
        };

        let manager = r2d2_sqlite::SqliteConnectionManager::memory();
        let pool = r2d2::Pool::builder().build(manager).unwrap();

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(!result.unwrap()); // rejected
    }

    // --- W7-T5: SEV-SNP measurement binding in WebAuthn challenge ---

    #[test]
    fn test_generate_challenge_with_sev_snp_is_deterministic() {
        let m = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let c1 = generate_challenge_with_sev_snp("cmd", "req", "scope", Some(m));
        let c2 = generate_challenge_with_sev_snp("cmd", "req", "scope", Some(m));
        assert_eq!(c1, c2);
        assert_eq!(c1.len(), 32);
    }

    #[test]
    fn test_generate_challenge_with_sev_snp_differs_per_measurement() {
        // Different measurement → different challenge. This is the
        // security property: if the gateway binary is modified (changing
        // the launch measurement), the WebAuthn challenge also changes,
        // so previously-issued assertions cannot be replayed.
        let m1 = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let m2 = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        let c1 = generate_challenge_with_sev_snp("cmd", "req", "scope", Some(m1));
        let c2 = generate_challenge_with_sev_snp("cmd", "req", "scope", Some(m2));
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_generate_challenge_with_sev_snp_none_matches_base() {
        // When measurement is None, the SEV-SNP variant must behave
        // identically to the base generate_challenge (no TEE binding).
        // This is the dev-box fallback.
        let base = generate_challenge("cmd", "req", "scope");
        let with_none = generate_challenge_with_sev_snp("cmd", "req", "scope", None);
        assert_eq!(base, with_none);
    }

    #[test]
    fn test_generate_challenge_with_sev_snp_some_differs_from_base() {
        // Binding a non-None measurement must change the challenge vs.
        // the un-bound version.
        let m = "sha256:3333333333333333333333333333333333333333333333333333333333333333";
        let base = generate_challenge("cmd", "req", "scope");
        let bound = generate_challenge_with_sev_snp("cmd", "req", "scope", Some(m));
        assert_ne!(base, bound);
    }

    #[test]
    fn test_generate_challenge_with_sev_snp_empty_measurement_string() {
        // An empty measurement string contributes zero bytes to the SHA-256
        // input, so it must yield the same challenge as `None`. This is
        // the correct behavior — the `Some("")` case is a degenerate input
        // and shouldn't pretend to bind anything.
        let base = generate_challenge("cmd", "req", "scope");
        let empty = generate_challenge_with_sev_snp("cmd", "req", "scope", Some(""));
        assert_eq!(base, empty);

        // But a non-empty measurement must differ from the empty/None case.
        let non_empty =
            generate_challenge_with_sev_snp("cmd", "req", "scope", Some("sha256:abc"));
        assert_ne!(empty, non_empty);
    }

    #[test]
    fn test_generate_challenge_with_sev_snp_differs_per_cmd_request_scope() {
        // The base triple (cmd/request/scope) still has the same binding
        // effect — different triples yield different challenges even
        // when the measurement is fixed.
        let m = "sha256:fixed-measurement";
        let c1 = generate_challenge_with_sev_snp("cmd1", "req", "scope", Some(m));
        let c2 = generate_challenge_with_sev_snp("cmd2", "req", "scope", Some(m));
        assert_ne!(c1, c2);

        let c3 = generate_challenge_with_sev_snp("cmd", "req1", "scope", Some(m));
        let c4 = generate_challenge_with_sev_snp("cmd", "req2", "scope", Some(m));
        assert_ne!(c3, c4);

        let c5 = generate_challenge_with_sev_snp("cmd", "req", "scope1", Some(m));
        let c6 = generate_challenge_with_sev_snp("cmd", "req", "scope2", Some(m));
        assert_ne!(c5, c6);
    }

    #[test]
    fn test_sev_snp_measurement_hash_is_deterministic() {
        let report = crate::tee::AttestationReport {
            report: "report-bytes".to_string(),
            report_hash: "abc123".to_string(),
            measurement: "sha256:deadbeef".to_string(),
            sev_snp_active: true,
            hardened_mode: true,
            generated_at: "2026-07-29T00:00:00Z".to_string(),
        };
        let h1 = sev_snp_measurement_hash(&report);
        let h2 = sev_snp_measurement_hash(&report);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_sev_snp_measurement_hash_differs_per_report() {
        let r1 = crate::tee::AttestationReport {
            report: "r1".to_string(),
            report_hash: "h1".to_string(),
            measurement: "sha256:m1".to_string(),
            sev_snp_active: true,
            hardened_mode: true,
            generated_at: "2026-07-29T00:00:00Z".to_string(),
        };
        let r2 = crate::tee::AttestationReport {
            report: "r2".to_string(),
            report_hash: "h2".to_string(),
            measurement: "sha256:m2".to_string(),
            sev_snp_active: true,
            hardened_mode: true,
            generated_at: "2026-07-29T00:00:00Z".to_string(),
        };
        assert_ne!(sev_snp_measurement_hash(&r1), sev_snp_measurement_hash(&r2));
    }

    // --- Property tests ---

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn proptest_challenge_deterministic(
            cmd_hash in proptest::prelude::any::<String>(),
            request_id in proptest::prelude::any::<String>(),
            scope_hash in proptest::prelude::any::<String>()
        ) {
            let c1 = generate_challenge(&cmd_hash, &request_id, &scope_hash);
            let c2 = generate_challenge(&cmd_hash, &request_id, &scope_hash);
            prop_assert_eq!(&c1, &c2);
            prop_assert_eq!(c1.len(), 32);
        }

        #[test]
        fn proptest_random_challenge_unique(
            _ in proptest::prelude::any::<u8>()
        ) {
            let c1 = generate_random_challenge();
            let c2 = generate_random_challenge();
            prop_assert_ne!(c1, c2);
        }

        #[test]
        fn proptest_challenge_with_sev_snp_deterministic(
            cmd_hash in proptest::prelude::any::<String>(),
            request_id in proptest::prelude::any::<String>(),
            scope_hash in proptest::prelude::any::<String>(),
            measurement in proptest::prelude::any::<String>()
        ) {
            let c1 = generate_challenge_with_sev_snp(
                &cmd_hash, &request_id, &scope_hash, Some(&measurement)
            );
            let c2 = generate_challenge_with_sev_snp(
                &cmd_hash, &request_id, &scope_hash, Some(&measurement)
            );
            prop_assert_eq!(&c1, &c2);
            prop_assert_eq!(c1.len(), 32);
        }

        #[test]
        fn proptest_challenge_with_sev_snp_differs_when_measurement_differs(
            cmd_hash in proptest::prelude::any::<String>(),
            request_id in proptest::prelude::any::<String>(),
            scope_hash in proptest::prelude::any::<String>(),
            m1 in proptest::prelude::any::<String>(),
            m2 in proptest::prelude::any::<String>()
        ) {
            if m1 != m2 {
                let c1 = generate_challenge_with_sev_snp(
                    &cmd_hash, &request_id, &scope_hash, Some(&m1)
                );
                let c2 = generate_challenge_with_sev_snp(
                    &cmd_hash, &request_id, &scope_hash, Some(&m2)
                );
                prop_assert_ne!(&c1, &c2);
            }
        }
    }
}
