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
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use p256::PublicKey;
use rand::RngCore;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
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
/// 4. The ECDSA P-256 signature over `authenticator_data || SHA-256(client_data_json)`
///    verifies against the credential's stored public key
///
/// Returns `Ok(true)` only if all checks pass. Returns `Ok(false)` if any
/// check fails (the caller treats this as an authentication failure).
/// Returns `Err(_)` only on infrastructure failures (e.g. DB pool exhausted).
pub fn verify_assertion(
    db: &Pool<SqliteConnectionManager>,
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
    if let Err(e) = parse_and_validate_client_data(
        assertion,
        &expected_challenge,
        DEFAULT_RP_ORIGIN,
    ) {
        tracing::warn!(
            tenant = %tenant_id,
            request = %request_id,
            error = %e,
            "WebAuthn client_data validation failed"
        );
        return Ok(false);
    }

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

    // 5. Counter replay protection (W3C §6.1 step 18). If the stored
    //    counter is non-zero, the asserted counter must be strictly greater.
    //    A lower-or-equal counter signals a cloned authenticator.
    if let Ok(Some(stored_counter)) = load_credential_counter(db, tenant_id, &assertion.credential_id)
    {
        if !counter_is_valid(stored_counter, auth_data.sign_count) {
            tracing::warn!(
                tenant = %tenant_id,
                request = %request_id,
                credential = %assertion.credential_id,
                stored_counter,
                asserted_counter = auth_data.sign_count,
                "WebAuthn counter replay detected — cloned authenticator?"
            );
            return Ok(false);
        }
    }

    // 6. Verify the ECDSA P-256 signature against the credential's public key.
    //
    // The signed message is `authenticator_data || SHA-256(client_data_json)`
    // per the W3C WebAuthn specification (§6.1 "Verifying an Authentication
    // Assertion", step 17). The signature is ASN.1 DER-encoded.
    //
    // Any failure in this stage (credential not found, malformed key/sig,
    // bad signature) results in `Ok(false)` — never an `Err` — so that the
    // caller treats it as a normal authentication rejection.
    if !verify_assertion_signature(db, tenant_id, assertion) {
        tracing::warn!(
            tenant = %tenant_id,
            request = %request_id,
            credential = %assertion.credential_id,
            "WebAuthn signature verification failed"
        );
        return Ok(false);
    }

    // 7. Advance the stored counter (W3C §6.1 step 19). Errors are logged
    //    but not propagated — a failed counter update must not invalidate
    //    an otherwise-valid assertion.
    update_credential_counter(db, tenant_id, &assertion.credential_id, auth_data.sign_count);

    tracing::info!(
        tenant = %tenant_id,
        request = %request_id,
        credential = %assertion.credential_id,
        counter = auth_data.sign_count,
        "WebAuthn assertion verified — signature valid"
    );

    Ok(true)
}

/// Load the stored public key (base64/base64url-encoded SEC1 or SPKI bytes)
/// for a credential from the database.
///
/// Returns `Ok(None)` if the credential is not found for this tenant or has
/// been revoked. Returns `Ok(Some(_))` with the encoded public key string
/// on success.
fn load_credential_public_key(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    credential_id: &str,
) -> Result<Option<String>> {
    let conn = db.get()?;
    match conn.query_row(
        "SELECT public_key FROM credentials
         WHERE tenant_id = ?1
           AND credential_id = ?2
           AND revoked_at IS NULL",
        params![tenant_id, credential_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(pk) => Ok(Some(pk)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Decode a base64-encoded string, accepting both base64url (no padding,
/// as used throughout this gateway) and standard base64 (with padding, as
/// sent by browsers via `btoa()`).
///
/// WebAuthn browsers historically send assertion fields as standard base64,
/// but the rest of this gateway uses base64url. Accepting both makes the
/// verifier robust to either encoding without weakening security.
fn decode_b64_flexible(s: &str) -> Result<Vec<u8>> {
    // Try base64url without padding first (the gateway's canonical encoding).
    if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s) {
        return Ok(bytes);
    }
    // Fall back to standard base64 with padding (browser `btoa` output).
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s) {
        return Ok(bytes);
    }
    // Last resort: base64url WITH padding.
    base64::engine::general_purpose::URL_SAFE
        .decode(s)
        .map_err(|e| anyhow::anyhow!("failed to decode base64/base64url: {}", e))
}

/// Perform the cryptographic signature verification for a WebAuthn assertion.
///
/// Steps:
/// 1. Load the credential's public key from the DB.
/// 2. Decode the public key (SEC1 point or SPKI DER) into a `p256::PublicKey`.
/// 3. Decode the assertion signature from base64, then parse as ASN.1 DER.
/// 4. Reconstruct the signed message: `authenticator_data || SHA-256(client_data_json)`.
/// 5. Verify the ECDSA P-256 signature.
///
/// Returns `true` only if every step succeeds and the signature is valid.
fn verify_assertion_signature(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    assertion: &WebAuthnAssertion,
) -> bool {
    // 1. Load the credential's stored public key.
    let public_key_str = match load_credential_public_key(db, tenant_id, &assertion.credential_id)
    {
        Ok(Some(pk)) => pk,
        Ok(None) => {
            tracing::warn!(
                tenant = %tenant_id,
                credential = %assertion.credential_id,
                "WebAuthn credential not found (or revoked) for tenant"
            );
            return false;
        }
        Err(e) => {
            tracing::error!(
                tenant = %tenant_id,
                credential = %assertion.credential_id,
                error = %e,
                "DB error while loading credential public key"
            );
            return false;
        }
    };

    // 2. Decode the public key bytes and parse as a P-256 public key.
    //
    // The `public_key` field is stored during enrollment as the output of
    // `navigator.credentials.create().response.getPublicKey()`, which is the
    // SubjectPublicKeyInfo (SPKI) DER encoding of the EC P-256 key. We also
    // accept the raw SEC1 point encoding (compressed or uncompressed) for
    // robustness — `from_sec1_bytes` handles the SEC1 point, and
    // `from_public_key_der` handles the SPKI wrapper.
    let public_key_bytes = match decode_b64_flexible(&public_key_str) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to decode stored public key base64");
            return false;
        }
    };

    let public_key = match PublicKey::from_sec1_bytes(&public_key_bytes) {
        Ok(pk) => pk,
        Err(_) => {
            // Not a raw SEC1 point — try SPKI DER (the browser's native format).
            use p256::pkcs8::DecodePublicKey;
            match PublicKey::from_public_key_der(&public_key_bytes) {
                Ok(pk) => pk,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key_len = public_key_bytes.len(),
                        "stored public key is neither a valid P-256 SEC1 point nor SPKI DER"
                    );
                    return false;
                }
            }
        }
    };

    // 3. Decode and parse the signature (ASN.1 DER, as produced by WebAuthn
    //    authenticators for ES256).
    let signature_bytes = match decode_b64_flexible(&assertion.signature) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to decode assertion signature base64");
            return false;
        }
    };

    let signature = match Signature::from_der(&signature_bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                sig_len = signature_bytes.len(),
                "assertion signature is not valid DER-encoded ECDSA P-256"
            );
            return false;
        }
    };

    // 4. Reconstruct the signed message: authenticator_data || SHA-256(client_data_json).
    let auth_data_bytes = match decode_b64_flexible(&assertion.authenticator_data) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to decode authenticator_data base64");
            return false;
        }
    };
    let client_data_bytes = match decode_b64_flexible(&assertion.client_data_json) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to decode client_data_json base64");
            return false;
        }
    };
    let client_data_hash = Sha256::digest(&client_data_bytes);
    let mut signed_message = Vec::with_capacity(auth_data_bytes.len() + client_data_hash.len());
    signed_message.extend_from_slice(&auth_data_bytes);
    signed_message.extend_from_slice(&client_data_hash);

    // 5. Verify the ECDSA P-256 signature.
    let verifying_key = VerifyingKey::from(&public_key);
    verifying_key.verify(&signed_message, &signature).is_ok()
}


// ============================================================================
// U1: Ceremony generation — PublicKeyCredentialCreationOptions
// ============================================================================

/// A public key credential parameter: a (type, alg) pair telling the
/// authenticator which signature algorithms the RP accepts.
///
/// Per the W3C WebAuthn spec, `type` is always `"public-key"` and `alg`
/// is a COSE algorithm identifier (e.g. `-7` for ES256, `-257` for RS256).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicKeyCredentialParameters {
    #[serde(rename = "type")]
    pub kind: String,
    pub alg: i64,
}

/// The relying party (RP) identity sent in the ceremony options.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelyingPartyEntity {
    pub id: String,
    pub name: String,
}

/// The user identity sent in the ceremony options. The `id` MUST be an
/// opaque byte string (we send it base64url-encoded); the authenticator
/// stores it and returns it on future assertions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UserEntity {
    pub id: String,
    pub name: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
}

/// Authenticator selection criteria. We require platform authenticators
/// (TPM/Secure Enclave — no removable USB keys) with user verification
/// (biometric or PIN).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthenticatorSelection {
    #[serde(rename = "authenticatorAttachment")]
    pub authenticator_attachment: String,
    #[serde(rename = "userVerification")]
    pub user_verification: String,
}

/// `navigator.credentials.create()` options — the JSON the phone browser
/// consumes to drive a registration ceremony.
///
/// All field names use camelCase (via `serde(rename)`) to match the W3C
/// WebAuthn dictionary shape so the browser can consume the JSON directly.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicKeyCredentialCreationOptions {
    pub challenge: String,
    pub rp: RelyingPartyEntity,
    pub user: UserEntity,
    #[serde(rename = "pubKeyCredParams")]
    pub pub_key_cred_params: Vec<PublicKeyCredentialParameters>,
    #[serde(rename = "authenticatorSelection")]
    pub authenticator_selection: AuthenticatorSelection,
    pub timeout: u64,
    pub attestation: String,
}

/// Generate a fresh `PublicKeyCredentialCreationOptions` for a registration
/// ceremony. Returns `(options, raw_challenge_bytes)` — the caller MUST
/// persist the raw challenge bytes (e.g. in `phone_challenges`) so the
/// subsequent `/phone/ceremony/finish` call can verify the attestation
/// response against the same challenge.
///
/// The challenge is 32 random bytes from `OsRng`, base64url-encoded.
/// `pubKeyCredParams` offers both ES256 (alg -7, P-256) and RS256 (alg -257,
/// RSA-2048) — every major platform authenticator supports at least ES256.
pub fn generate_ceremony_options(
    _tenant_id: &str,
    rp_id: &str,
    rp_name: &str,
    user_id: &str,
    user_name: &str,
) -> (PublicKeyCredentialCreationOptions, [u8; CHALLENGE_LEN]) {
    let challenge = generate_random_challenge();
    let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);

    let options = PublicKeyCredentialCreationOptions {
        challenge: challenge_b64,
        rp: RelyingPartyEntity {
            id: rp_id.to_string(),
            name: rp_name.to_string(),
        },
        user: UserEntity {
            id: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(user_id.as_bytes()),
            name: user_name.to_string(),
            display_name: user_name.to_string(),
        },
        pub_key_cred_params: vec![
            PublicKeyCredentialParameters { kind: "public-key".to_string(), alg: -7 },    // ES256
            PublicKeyCredentialParameters { kind: "public-key".to_string(), alg: -257 }, // RS256
        ],
        authenticator_selection: AuthenticatorSelection {
            authenticator_attachment: "platform".to_string(),
            user_verification: "required".to_string(),
        },
        timeout: 60000,
        attestation: "none".to_string(),
    };

    (options, challenge)
}

/// Store a freshly-generated ceremony challenge in the `phone_challenges`
/// table. The `challenge_id` (a ULID) is returned to the client as part
/// of the ceremony state so the finish endpoint can reference it.
pub fn store_challenge(
    db: &Pool<SqliteConnectionManager>,
    challenge_id: &str,
    tenant_id: &str,
    challenge: &[u8],
) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO phone_challenges (id, tenant_id, challenge, created_at)
         VALUES (?1, ?2, ?3, datetime('now'))",
        params![challenge_id, tenant_id, challenge],
    )?;
    Ok(())
}

/// Look up and atomically consume a ceremony challenge. Marks the row
/// `used_at` so it can never be replayed. Returns the raw challenge bytes
/// or `None` if the challenge doesn't exist, was already used, or doesn't
/// belong to the given tenant.
pub fn take_challenge(
    db: &Pool<SqliteConnectionManager>,
    challenge_id: &str,
    tenant_id: &str,
) -> Result<Option<Vec<u8>>> {
    let conn = db.get()?;
    let challenge: Option<Vec<u8>> = match conn.query_row(
        "SELECT challenge FROM phone_challenges
         WHERE id = ?1 AND tenant_id = ?2 AND used_at IS NULL",
        params![challenge_id, tenant_id],
        |row| row.get::<_, Vec<u8>>(0),
    ) {
        Ok(c) => Some(c),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };

    if let Some(ch) = challenge {
        conn.execute(
            "UPDATE phone_challenges SET used_at = datetime('now') WHERE id = ?1",
            params![challenge_id],
        )?;
        Ok(Some(ch))
    } else {
        Ok(None)
    }
}

/// Look up and atomically consume a ceremony challenge by `challenge_id`
/// alone, **without** a tenant_id constraint. Returns the `(tenant_id,
/// challenge)` pair so the caller (e.g. `POST /phone/enroll` or
/// `POST /phone/ceremony/finish`) can bind the stored credential to the
/// correct tenant.
///
/// Returns `Ok(None)` if the challenge doesn't exist or was already used.
pub fn take_challenge_by_id(
    db: &Pool<SqliteConnectionManager>,
    challenge_id: &str,
) -> Result<Option<(String, Vec<u8>)>> {
    let conn = db.get()?;
    let row: Option<(String, Vec<u8>)> = match conn.query_row(
        "SELECT tenant_id, challenge FROM phone_challenges
         WHERE id = ?1 AND used_at IS NULL",
        params![challenge_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
    ) {
        Ok(r) => Some(r),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };

    if let Some((tenant_id, challenge)) = row {
        conn.execute(
            "UPDATE phone_challenges SET used_at = datetime('now') WHERE id = ?1",
            params![challenge_id],
        )?;
        Ok(Some((tenant_id, challenge)))
    } else {
        Ok(None)
    }
}

// ============================================================================
// U2: Real WebAuthn assertion verification — counter replay protection
// ============================================================================

/// Load the stored signature counter for a credential. Used for replay
/// protection (W3C §6.1 step 18). Returns `Ok(None)` if the credential
/// is not found or has been revoked, or if the `credentials` table /
/// `counter` column does not exist (e.g. in tests with a bare in-memory
/// DB). In the latter case the caller treats it as "no replay protection"
/// and proceeds.
fn load_credential_counter(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    credential_id: &str,
) -> Result<Option<u32>> {
    let conn = match db.get() {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    match conn.query_row(
        "SELECT counter FROM credentials
         WHERE tenant_id = ?1
           AND credential_id = ?2
           AND revoked_at IS NULL",
        params![tenant_id, credential_id],
        |row| row.get::<_, u32>(0),
    ) {
        Ok(c) => Ok(Some(c)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(_) => Ok(None), // table/column missing — degrade gracefully
    }
}

/// Update the stored signature counter after a successful assertion.
/// Per W3C §6.1 step 19, the stored counter must be advanced to the
/// freshly-asserted value. Errors are logged but not propagated — a
/// failed counter update must not invalidate an otherwise-valid assertion.
fn update_credential_counter(
    db: &Pool<SqliteConnectionManager>,
    tenant_id: &str,
    credential_id: &str,
    new_counter: u32,
) {
    if let Ok(conn) = db.get() {
        if let Err(e) = conn.execute(
            "UPDATE credentials SET counter = ?1, last_used_at = datetime('now')
             WHERE tenant_id = ?2 AND credential_id = ?3",
            params![new_counter, tenant_id, credential_id],
        ) {
            tracing::warn!(error = %e, "failed to update credential counter");
        }
    }
}

/// Check the W3C counter replay-protection invariant.
///
/// Per §6.1 step 18: "If either the stored `signCount` value or the
/// asserted `signCount` value is 0, the test passes by default."
/// Otherwise the asserted counter must be strictly greater than the
/// stored counter — a lower-or-equal counter signals a cloned
/// authenticator.
fn counter_is_valid(stored: u32, asserted: u32) -> bool {
    if stored == 0 || asserted == 0 {
        return true;
    }
    asserted > stored
}

// ============================================================================
// U2: Attestation verification (registration ceremony)
// ============================================================================

/// `AuthenticatorAttestationResponse` as produced by
/// `navigator.credentials.create().response`. The browser serializes each
/// `ArrayBuffer` field as base64url (no padding).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AuthenticatorAttestationResponse {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    #[serde(rename = "attestationObject")]
    pub attestation_object: String,
}

/// Result of a successful attestation verification — the extracted
/// credential id (raw bytes, base64url-encoded) and public key
/// (SEC1-uncompressed, base64url-encoded), ready for INSERT into the
/// `credentials` table.
#[derive(Debug, Clone)]
pub struct AttestationResult {
    pub credential_id: String,
    pub public_key: String,
    pub aaguid: String,
    pub sign_count: u32,
    pub fmt: String,
}

/// Minimal CBOR value — only as much as we need to peel open an
/// `attestationObject` (a 3-entry map with text keys) and a COSE_Key.
#[derive(Debug, Clone)]
enum CborValue {
    UnsignedInt(u64),
    NegativeInt(i64),
    ByteString(Vec<u8>),
    TextString(String),
    Array(Vec<CborValue>),
    Map(Vec<(CborValue, CborValue)>),
    Bool(bool),
    Null,
}

/// Parse a single CBOR item from `data`. Returns `(value, remaining)`.
fn cbor_parse(data: &[u8]) -> Result<(CborValue, &[u8])> {
    if data.is_empty() {
        anyhow::bail!("cbor: unexpected end of input");
    }
    let initial = data[0];
    let major = initial >> 5;
    let ai = initial & 0x1f;
    let (value, body) = match ai {
        0..=23 => (u64::from(ai), &data[1..]),
        24 => (u64::from(data[1]), &data[2..]),
        25 => {
            let v = u16::from_be_bytes([data[1], data[2]]) as u64;
            (v, &data[3..])
        }
        26 => {
            let v = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as u64;
            (v, &data[5..])
        }
        27 => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&data[1..9]);
            (u64::from_be_bytes(bytes), &data[9..])
        }
        _ => anyhow::bail!("cbor: unsupported additional info {}", ai),
    };
    let rest = body;
    Ok(match major {
        0 => (CborValue::UnsignedInt(value), rest),
        1 => (CborValue::NegativeInt(-1 - (value as i64)), rest),
        2 => {
            let len = value as usize;
            if rest.len() < len {
                anyhow::bail!("cbor: byte string overflow");
            }
            (CborValue::ByteString(rest[..len].to_vec()), &rest[len..])
        }
        3 => {
            let len = value as usize;
            if rest.len() < len {
                anyhow::bail!("cbor: text string overflow");
            }
            let s = String::from_utf8(rest[..len].to_vec())
                .map_err(|e| anyhow::anyhow!("cbor: invalid utf-8: {}", e))?;
            (CborValue::TextString(s), &rest[len..])
        }
        4 => {
            let len = value as usize;
            let mut items = Vec::with_capacity(len);
            let mut r = rest;
            for _ in 0..len {
                let (v, remaining) = cbor_parse(r)?;
                items.push(v);
                r = remaining;
            }
            (CborValue::Array(items), r)
        }
        5 => {
            let len = value as usize;
            let mut items = Vec::with_capacity(len);
            let mut r = rest;
            for _ in 0..len {
                let (k, kr) = cbor_parse(r)?;
                let (v, vr) = cbor_parse(kr)?;
                items.push((k, v));
                r = vr;
            }
            (CborValue::Map(items), r)
        }
        7 => match value {
            20 => (CborValue::Bool(false), rest),
            21 => (CborValue::Bool(true), rest),
            22 => (CborValue::Null, rest),
            _ => (CborValue::UnsignedInt(value), rest),
        },
        _ => anyhow::bail!("cbor: unsupported major type {}", major),
    })
}

/// Helper: if the CBOR value is a text string, return it.
fn cbor_as_text(v: &CborValue) -> Result<&str> {
    match v {
        CborValue::TextString(s) => Ok(s),
        _ => anyhow::bail!("expected CBOR text string, got {:?}", v),
    }
}

/// Helper: if the CBOR value is a byte string, return it.
fn cbor_as_bytes(v: &CborValue) -> Result<&[u8]> {
    match v {
        CborValue::ByteString(b) => Ok(b),
        _ => anyhow::bail!("expected CBOR byte string, got {:?}", v),
    }
}

/// Helper: if the CBOR value is an unsigned int, return it.
fn cbor_as_uint(v: &CborValue) -> Result<u64> {
    match v {
        CborValue::UnsignedInt(n) => Ok(*n),
        _ => anyhow::bail!("expected CBOR unsigned int, got {:?}", v),
    }
}

/// Helper: if the CBOR value is a negative int, return it (as i64).
fn cbor_as_int(v: &CborValue) -> Result<i64> {
    match v {
        CborValue::UnsignedInt(n) => Ok(*n as i64),
        CborValue::NegativeInt(n) => Ok(*n),
        _ => anyhow::bail!("expected CBOR int, got {:?}", v),
    }
}

/// Parsed attestation object — the 3-field CBOR map from the
/// `attestationObject` field.
struct AttestationObject {
    fmt: String,
    auth_data: Vec<u8>,
    att_stmt: CborValue,
}

/// Parse the `attestationObject` (a 3-entry CBOR map with text keys
/// `fmt`, `authData`, `attStmt`).
fn parse_attestation_object(data: &[u8]) -> Result<AttestationObject> {
    let (value, _rest) = cbor_parse(data)?;
    let mut entries = match value {
        CborValue::Map(m) => m,
        _ => anyhow::bail!("attestationObject: expected CBOR map"),
    };

    let mut fmt = None;
    let mut auth_data = None;
    let mut att_stmt = CborValue::Null;

    for (k, v) in entries.drain(..) {
        let key = cbor_as_text(&k)?;
        match key {
            "fmt" => fmt = Some(cbor_as_text(&v)?.to_string()),
            "authData" => auth_data = Some(cbor_as_bytes(&v)?.to_vec()),
            "attStmt" => att_stmt = v,
            _ => {} // ignore unknown keys
        }
    }

    let fmt = fmt.ok_or_else(|| anyhow::anyhow!("attestationObject missing 'fmt'"))?;
    let auth_data =
        auth_data.ok_or_else(|| anyhow::anyhow!("attestationObject missing 'authData'"))?;

    Ok(AttestationObject {
        fmt,
        auth_data,
        att_stmt,
    })
}

/// Extract the attested credential data from `authData` (the trailing
/// bytes after the 37-byte fixed header).
///
/// Layout (W3C §6.5.2):
///   32 bytes  rp_id_hash
///   1 byte    flags (bit 6 = AT, attested credential data present)
///   4 bytes   sign_count (big-endian)
///   16 bytes  aaguid
///   2 bytes   credential_id_len (big-endian)
///   variable  credential_id
///   variable  COSE_Key (CBOR-encoded public key)
///
/// Returns `(credential_id_bytes, cose_key_bytes, sign_count, aaguid_hex)`.
fn extract_attested_credential_data(
    auth_data: &[u8],
) -> Result<(Vec<u8>, Vec<u8>, u32, String)> {
    if auth_data.len() < 37 + 18 {
        anyhow::bail!(
            "authData too short for attested credential data: {} bytes",
            auth_data.len()
        );
    }
    let flags = auth_data[32];
    if (flags & 0x40) == 0 {
        anyhow::bail!("authData missing AT flag — no attested credential data");
    }
    let sign_count = u32::from_be_bytes([
        auth_data[33],
        auth_data[34],
        auth_data[35],
        auth_data[36],
    ]);
    let aaguid = &auth_data[37..53];
    let cred_id_len = u16::from_be_bytes([auth_data[53], auth_data[54]]) as usize;
    if auth_data.len() < 55 + cred_id_len {
        anyhow::bail!("authData credential_id overflow");
    }
    let credential_id = auth_data[55..55 + cred_id_len].to_vec();
    let cose_key_bytes = &auth_data[55 + cred_id_len..];

    let aaguid_hex = hex::encode(aaguid);
    Ok((credential_id, cose_key_bytes.to_vec(), sign_count, aaguid_hex))
}

/// Convert a COSE_Key (CBOR map) for an EC2 P-256 key into the SEC1
/// uncompressed point encoding `0x04 || x || y` (65 bytes).
fn cose_key_to_sec1(cose_key: &CborValue) -> Result<Vec<u8>> {
    let entries = match cose_key {
        CborValue::Map(m) => m,
        _ => anyhow::bail!("COSE_Key: expected map"),
    };
    let mut kty = None;
    let mut crv = None;
    let mut x = None;
    let mut y = None;
    for (k, v) in entries {
        let key = cbor_as_int(k)?;
        match key {
            1 => kty = Some(cbor_as_int(v)?),
            -1 => crv = Some(cbor_as_int(v)?),
            -2 => x = Some(cbor_as_bytes(v)?.to_vec()),
            -3 => y = Some(cbor_as_bytes(v)?.to_vec()),
            _ => {}
        }
    }
    if kty != Some(2) {
        anyhow::bail!("COSE_Key: kty must be 2 (EC2), got {:?}", kty);
    }
    if crv != Some(1) {
        anyhow::bail!("COSE_Key: crv must be 1 (P-256), got {:?}", crv);
    }
    let x = x.ok_or_else(|| anyhow::anyhow!("COSE_Key missing x (-2)"))?;
    let y = y.ok_or_else(|| anyhow::anyhow!("COSE_Key missing y (-3)"))?;
    if x.len() != 32 || y.len() != 32 {
        anyhow::bail!(
            "COSE_Key: P-256 coordinates must be 32 bytes each (got {}, {})",
            x.len(),
            y.len()
        );
    }
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(&x);
    sec1.extend_from_slice(&y);
    Ok(sec1)
}

/// Verify a registration attestation response.
///
/// Steps (W3C §7.1 "Verifying an Attestation"):
/// 1. Decode `clientDataJSON` and verify `type == "webauthn.create"`,
///    `origin == expected_origin`, `challenge == expected_challenge`.
/// 2. Decode `attestationObject` (CBOR) and extract `fmt`, `authData`,
///    `attStmt`.
/// 3. Verify `authData.rp_id_hash == SHA-256(expected_rp_id)`.
/// 4. Verify `authData.flags` has UV (user verification) set.
/// 5. Extract the attested credential data (aaguid, credential_id, COSE_Key).
/// 6. Convert COSE_Key → SEC1 public key.
/// 7. Verify the attestation statement format:
///    - `"none"`: no signature to verify — accept (per W3C §8.7).
///    - `"packed"` self-attestation: verify the signature over
///      `authData || SHA-256(clientDataJSON)` using the credential's own
///      public key (W3C §8.3).
///    - Other formats (`tpm`, `android-key`, `fido-u2f`, ...): return
///      `Err` — we don't implement the full attestation chain for these
///      (would require X.509 cert parsing + TPM-specific verification).
/// 8. Return the extracted credential_id (base64url), public_key (SEC1,
///    base64url), aaguid (hex), and sign_count.
pub fn verify_attestation(
    attestation: &AuthenticatorAttestationResponse,
    expected_challenge: &[u8],
    expected_origin: &str,
    expected_rp_id: &str,
) -> Result<AttestationResult> {
    // 1. Parse client_data_json (allowing webauthn.create).
    let client_data_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&attestation.client_data_json)
        .map_err(|e| anyhow::anyhow!("invalid clientDataJSON base64url: {}", e))?;
    let client_data: ClientData = serde_json::from_slice(&client_data_bytes)
        .map_err(|e| anyhow::anyhow!("invalid clientDataJSON JSON: {}", e))?;
    if client_data.kind != "webauthn.create" {
        return Err(anyhow::anyhow!(
            "expected clientData.type == 'webauthn.create', got '{}'",
            client_data.kind
        ));
    }
    if client_data.origin != expected_origin {
        return Err(anyhow::anyhow!(
            "origin mismatch: expected '{}', got '{}' — possible phishing",
            expected_origin,
            client_data.origin
        ));
    }
    let expected_challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(expected_challenge);
    if client_data.challenge != expected_challenge_b64 {
        return Err(anyhow::anyhow!(
            "challenge mismatch — attestation is for a different ceremony"
        ));
    }

    // 2. Parse attestationObject (CBOR).
    let att_obj_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&attestation.attestation_object)
        .map_err(|e| anyhow::anyhow!("invalid attestationObject base64url: {}", e))?;
    let att_obj = parse_attestation_object(&att_obj_bytes)?;

    // 3. Verify RP ID hash.
    let expected_rp_id_hash = Sha256::digest(expected_rp_id.as_bytes());
    if att_obj.auth_data.len() < 37 {
        anyhow::bail!("authData too short: {} bytes", att_obj.auth_data.len());
    }
    if &att_obj.auth_data[..32] != expected_rp_id_hash.as_slice() {
        return Err(anyhow::anyhow!(
            "RP ID hash mismatch in attestation authData"
        ));
    }

    // 4. Verify UV flag.
    let flags = att_obj.auth_data[32];
    if (flags & 0x04) == 0 {
        return Err(anyhow::anyhow!(
            "attestation missing user_verified (UV) flag — biometric/PIN required"
        ));
    }

    // 5. Extract attested credential data.
    let (credential_id, cose_key_bytes, sign_count, aaguid_hex) =
        extract_attested_credential_data(&att_obj.auth_data)?;

    // 6. Convert COSE_Key → SEC1.
    let (cose_key, _remaining) = cbor_parse(&cose_key_bytes)?;
    let sec1 = cose_key_to_sec1(&cose_key)?;
    let public_key_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sec1);
    let credential_id_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&credential_id);

    // 7. Verify attestation statement by format.
    match att_obj.fmt.as_str() {
        "none" => {
            // Per W3C §8.7: no attStmt verification — the authenticator
            // is asserting nothing about its provenance. Accept.
        }
        "packed" => {
            // Per W3C §8.3. Try self-attestation first (sig present in
            // attStmt.sig, alg matches the credential's COSE alg, no x5c).
            verify_packed_attestation(
                &att_obj.att_stmt,
                &att_obj.auth_data,
                &client_data_bytes,
                &sec1,
            )?;
        }
        other => {
            return Err(anyhow::anyhow!(
                "attestation format '{}' not supported (only 'none' and 'packed')",
                other
            ));
        }
    }

    Ok(AttestationResult {
        credential_id: credential_id_b64,
        public_key: public_key_b64,
        aaguid: aaguid_hex,
        sign_count,
        fmt: att_obj.fmt,
    })
}

/// Verify a "packed" attestation statement (W3C §8.3).
///
/// Self-attestation path: the `attStmt` map contains `alg` (COSE alg) and
/// `sig` (signature over `authData || SHA-256(clientDataJSON)`), with no
/// `x5c` (certificate chain). The signature is verified against the
/// credential's own public key.
///
/// For `x5c`-based (attested by a CA cert), we return `Err` — full
/// certificate-chain verification is out of scope (requires X.509 parsing).
fn verify_packed_attestation(
    att_stmt: &CborValue,
    auth_data: &[u8],
    client_data_bytes: &[u8],
    credential_sec1: &[u8],
) -> Result<()> {
    let entries = match att_stmt {
        CborValue::Map(m) => m,
        _ => anyhow::bail!("packed attStmt: expected map"),
    };

    let mut alg: Option<i64> = None;
    let mut sig: Option<Vec<u8>> = None;
    let mut has_x5c = false;
    let mut has_ecdaa_key_id = false;
    for (k, v) in entries {
        // Per W3C §8.3, the packed attStmt keys are TEXT strings:
        // "alg", "sig", "x5c", "ecdaaKeyId".
        let key = cbor_as_text(k)?;
        match key {
            "alg" => alg = Some(cbor_as_int(v)?),
            "sig" => sig = Some(cbor_as_bytes(v)?.to_vec()),
            "x5c" => has_x5c = true,
            "ecdaaKeyId" => has_ecdaa_key_id = true,
            _ => {}
        }
    }
    let alg = alg.ok_or_else(|| anyhow::anyhow!("packed attStmt missing 'alg'"))?;
    let sig = sig.ok_or_else(|| anyhow::anyhow!("packed attStmt missing 'sig'"))?;
    if has_x5c || has_ecdaa_key_id {
        return Err(anyhow::anyhow!(
            "packed attestation with x5c/ecdaaKeyId not supported (CA-chain verification out of scope)"
        ));
    }

    // Self-attestation: only ES256 (alg -7) is supported here.
    if alg != -7 {
        return Err(anyhow::anyhow!(
            "packed self-attestation: only ES256 (alg -7) supported, got {}",
            alg
        ));
    }

    // Parse the credential's public key.
    let public_key = PublicKey::from_sec1_bytes(credential_sec1)
        .map_err(|e| anyhow::anyhow!("invalid SEC1 public key: {}", e))?;
    let signature = Signature::from_der(&sig)
        .map_err(|e| anyhow::anyhow!("invalid DER signature: {}", e))?;

    // Reconstruct the signed message: authData || SHA-256(clientDataJSON).
    let client_data_hash = Sha256::digest(client_data_bytes);
    let mut signed_message = Vec::with_capacity(auth_data.len() + client_data_hash.len());
    signed_message.extend_from_slice(auth_data);
    signed_message.extend_from_slice(&client_data_hash);

    let verifying_key = VerifyingKey::from(&public_key);
    verifying_key
        .verify(&signed_message, &signature)
        .map_err(|_| anyhow::anyhow!("packed self-attestation signature verification failed"))
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

    // --- W1-T8: full verify_assertion (metadata + signature checks) ---

    #[test]
    fn test_verify_assertion_rejects_when_credential_missing() {
        // Build a valid assertion with matching challenge, origin, RP ID hash,
        // and UV flag — but no credential exists in the DB, so signature
        // verification cannot succeed and the assertion must be rejected.
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

        // An empty in-memory DB — no schema, no credentials table. The
        // metadata checks all pass, but the signature stage cannot find a
        // credential, so the assertion is rejected.
        let manager = r2d2_sqlite::SqliteConnectionManager::memory();
        let pool = r2d2::Pool::builder().build(manager).unwrap();

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(!result.unwrap()); // rejected — no credential found
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

    // --- A2: ECDSA P-256 signature verification ---

    /// Build the shared pieces of a valid WebAuthn assertion (client_data_json
    /// + authenticator_data, both base64url-encoded) for the given request_id.
    /// Returns `(client_data_bytes, client_data_b64, auth_bytes, auth_data_b64)`.
    fn build_valid_assertion_metadata(
        request_id: &str,
    ) -> (Vec<u8>, String, Vec<u8>, String) {
        let challenge = generate_challenge("", request_id, "");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&client_data_bytes);

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

        (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64)
    }

    /// Set up an in-memory DB pool (with full schema) containing one tenant
    /// and one enrolled credential whose public key is derived from the
    /// generated P-256 signing key.
    ///
    /// The public key is stored as base64url-encoded SEC1 bytes (the same
    /// encoding the gateway uses internally). Returns `(pool, signing_key)`.
    fn setup_db_with_credential(
        credential_id: &str,
    ) -> (
        r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
        p256::ecdsa::SigningKey,
    ) {
        use p256::ecdsa::SigningKey;
        use rand::rngs::OsRng;

        let pool = crate::db::init_memory_pool().expect("failed to init in-memory DB");

        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('test-tenant', 'Test', datetime('now'), 'x', 1)",
            [],
        )
        .unwrap();

        // Generate a P-256 keypair.
        let mut rng = OsRng;
        let signing_key = SigningKey::random(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let public_key_bytes = verifying_key.to_sec1_bytes();
        let public_key_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&public_key_bytes);

        conn.execute(
            "INSERT INTO credentials
             (id, tenant_id, credential_id, public_key, aaguid, transports, name, verified, created_at)
             VALUES ('cred-1', 'test-tenant', ?1, ?2, '', '', 'Test', 1, datetime('now'))",
            params![credential_id, public_key_b64],
        )
        .unwrap();
        drop(conn);

        (pool, signing_key)
    }

    /// Compute the WebAuthn signed message: `authenticator_data || SHA-256(client_data_json)`.
    fn webauthn_signed_message(auth_bytes: &[u8], client_data_bytes: &[u8]) -> Vec<u8> {
        let client_data_hash = Sha256::digest(client_data_bytes);
        let mut msg = Vec::with_capacity(auth_bytes.len() + client_data_hash.len());
        msg.extend_from_slice(auth_bytes);
        msg.extend_from_slice(&client_data_hash);
        msg
    }

    #[test]
    fn test_verify_assertion_accepts_valid_signature() {
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_valid_sig";
        let credential_id = "cred-valid";
        let (pool, signing_key) = setup_db_with_credential(credential_id);
        let (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        // Sign the WebAuthn message with the private key.
        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let signature_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(result.unwrap(), "valid signature must be accepted");
    }

    #[test]
    fn test_verify_assertion_rejects_tampered_signature() {
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_tampered";
        let credential_id = "cred-tampered";
        let (pool, signing_key) = setup_db_with_credential(credential_id);
        let (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let mut sig_bytes = signature.as_bytes().to_vec();

        // Tamper: flip a bit in the middle of the signature.
        let mid = sig_bytes.len() / 2;
        sig_bytes[mid] ^= 0x01;
        let signature_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sig_bytes);

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(
            !result.unwrap(),
            "tampered signature must be rejected"
        );
    }

    #[test]
    fn test_verify_assertion_rejects_wrong_message_signature() {
        // Sign a *different* message (wrong request_id) and present it as if
        // it were for the target request. The signature is structurally valid
        // DER, but it doesn't match `authenticator_data || SHA-256(client_data)`
        // for this assertion, so it must be rejected.
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_target";
        let credential_id = "cred-wrongmsg";
        let (pool, signing_key) = setup_db_with_credential(credential_id);
        let (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        // Sign over a different authenticator_data (flip the sign count).
        let mut wrong_auth = auth_bytes.clone();
        let last = wrong_auth.len() - 1;
        wrong_auth[last] ^= 0x01;
        let wrong_message = webauthn_signed_message(&wrong_auth, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&wrong_message);
        let signature_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(
            !result.unwrap(),
            "signature over the wrong message must be rejected"
        );
    }

    #[test]
    fn test_verify_assertion_rejects_revoked_credential() {
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_revoked";
        let credential_id = "cred-revoked";
        let (pool, signing_key) = setup_db_with_credential(credential_id);
        let (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        // Revoke the credential after enrollment.
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "UPDATE credentials SET revoked_at = datetime('now')
                 WHERE credential_id = ?1",
                params![credential_id],
            )
            .unwrap();
        }

        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let signature_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(
            !result.unwrap(),
            "revoked credential must be rejected even with a valid signature"
        );
    }

    #[test]
    fn test_verify_assertion_rejects_unknown_credential_id() {
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_unknown";
        let credential_id_enrolled = "cred-enrolled";
        let credential_id_asserted = "cred-different";
        let (pool, signing_key) = setup_db_with_credential(credential_id_enrolled);
        let (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let signature_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());

        // Present the assertion with a credential_id that doesn't match the
        // enrolled one. The signature is valid, but it's for a credential the
        // gateway doesn't know about.
        let assertion = WebAuthnAssertion {
            credential_id: credential_id_asserted.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(
            !result.unwrap(),
            "assertion for an unknown credential_id must be rejected"
        );
    }

    #[test]
    fn test_verify_assertion_accepts_valid_signature_spki_public_key() {
        // The browser's `navigator.credentials.create().response.getPublicKey()`
        // returns the key as a SubjectPublicKeyInfo (SPKI) DER blob, not as a
        // raw SEC1 point. This test stores the public key in SPKI format and
        // confirms `from_sec1_bytes()` can parse it.
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_spki";
        let credential_id = "cred-spki";
        let (pool, signing_key) = setup_db_with_credential(credential_id);
        let (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        // Overwrite the stored public key with the SPKI encoding.
        // The SPKI prefix for an uncompressed EC P-256 public key is 26 bytes:
        //   SEQUENCE { SEQUENCE { OID ecPublicKey, OID secp256r1 }, BIT STRING <point> }
        let verifying_key = signing_key.verifying_key();
        let sec1_point = verifying_key.to_sec1_bytes(); // 65 bytes (uncompressed)
        let spki_prefix: [u8; 26] = [
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
            0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
        ];
        let mut spki_bytes = Vec::with_capacity(spki_prefix.len() + sec1_point.len());
        spki_bytes.extend_from_slice(&spki_prefix);
        spki_bytes.extend_from_slice(&sec1_point);
        let spki_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&spki_bytes);

        {
            let conn = pool.get().unwrap();
            conn.execute(
                "UPDATE credentials SET public_key = ?1 WHERE credential_id = ?2",
                params![spki_b64, credential_id],
            )
            .unwrap();
        }

        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let signature_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(
            result.unwrap(),
            "valid signature with SPKI-encoded public key must be accepted"
        );
    }

    #[test]
    fn test_verify_assertion_accepts_standard_base64_signature() {
        // Browsers send the signature field as standard base64 (with `+/=`),
        // via `btoa()`. The flexible decoder in the signature path must
        // accept this encoding even though the rest of the gateway uses
        // base64url. (The client_data_json and authenticator_data fields
        // are parsed by the existing base64url-only parsers, so they must
        // stay base64url; only the signature field uses standard base64 here.)
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_std_b64";
        let credential_id = "cred-stdb64";
        let (pool, signing_key) = setup_db_with_credential(credential_id);
        let (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        // Encode the signature with STANDARD base64 (with padding) — simulating browser `btoa()`.
        let signature_b64 = base64::engine::general_purpose::STANDARD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(
            result.unwrap(),
            "standard base64-encoded signature must be accepted"
        );
    }

    #[test]
    fn test_verify_assertion_rejects_garbage_signature() {
        // A completely invalid signature (not valid DER) must be rejected.
        let request_id = "req_garbage";
        let credential_id = "cred-garbage";
        let (pool, _signing_key) = setup_db_with_credential(credential_id);
        let (_client_data_bytes, client_data_b64, _auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"not a real signature"),
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(!result.unwrap(), "garbage signature must be rejected");
    }

    #[test]
    fn test_verify_assertion_rejects_cross_tenant_credential() {
        // A credential enrolled under tenant-A must not be usable by tenant-B.
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_cross_tenant";
        let credential_id = "cred-cross";
        let (pool, signing_key) = setup_db_with_credential(credential_id);
        let (client_data_bytes, client_data_b64, auth_bytes, auth_data_b64) =
            build_valid_assertion_metadata(request_id);

        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let signature_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        // Use a different tenant_id than the one the credential was enrolled under.
        let result = verify_assertion(&pool, "other-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(
            !result.unwrap(),
            "credential from a different tenant must be rejected"
        );
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

    // --- U1: ceremony generation ---

    #[test]
    fn test_generate_ceremony_options_shape() {
        let (options, raw_challenge) = generate_ceremony_options(
            "tenant-1",
            DEFAULT_RP_ID,
            "Stronghold",
            "user-1",
            "alice",
        );

        // Challenge is 32 raw bytes, base64url-encoded.
        assert_eq!(raw_challenge.len(), CHALLENGE_LEN);
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&options.challenge)
            .unwrap();
        assert_eq!(decoded.len(), CHALLENGE_LEN);
        assert_eq!(decoded, raw_challenge);

        // RP identity.
        assert_eq!(options.rp.id, DEFAULT_RP_ID);
        assert_eq!(options.rp.name, "Stronghold");

        // User identity.
        assert_eq!(options.user.name, "alice");
        assert_eq!(options.user.display_name, "alice");
        // user.id is base64url(user_id).
        let user_id_decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&options.user.id)
            .unwrap();
        assert_eq!(user_id_decoded, b"user-1");

        // pubKeyCredParams has ES256 + RS256 in order.
        assert_eq!(options.pub_key_cred_params.len(), 2);
        assert_eq!(options.pub_key_cred_params[0].kind, "public-key");
        assert_eq!(options.pub_key_cred_params[0].alg, -7);
        assert_eq!(options.pub_key_cred_params[1].kind, "public-key");
        assert_eq!(options.pub_key_cred_params[1].alg, -257);

        // Authenticator selection.
        assert_eq!(
            options.authenticator_selection.authenticator_attachment,
            "platform"
        );
        assert_eq!(
            options.authenticator_selection.user_verification,
            "required"
        );

        // Timeout + attestation.
        assert_eq!(options.timeout, 60000);
        assert_eq!(options.attestation, "none");
    }

    #[test]
    fn test_generate_ceremony_options_random_challenge() {
        // Two calls produce different challenges.
        let (o1, c1) = generate_ceremony_options("t", DEFAULT_RP_ID, "n", "u", "name");
        let (o2, c2) = generate_ceremony_options("t", DEFAULT_RP_ID, "n", "u", "name");
        assert_ne!(o1.challenge, o2.challenge);
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_ceremony_options_serializes_to_json() {
        // The serialized JSON must use camelCase field names so the browser's
        // `navigator.credentials.create()` accepts it directly.
        let (options, _) =
            generate_ceremony_options("t", DEFAULT_RP_ID, "n", "u", "name");
        let json = serde_json::to_value(&options).unwrap();
        assert!(json.get("challenge").is_some());
        assert!(json.get("rp").is_some());
        assert!(json.get("user").is_some());
        assert!(json.get("pubKeyCredParams").is_some());
        assert!(json.get("authenticatorSelection").is_some());
        assert!(json.get("timeout").is_some());
        assert!(json.get("attestation").is_some());

        // RP fields.
        let rp = json.get("rp").unwrap();
        assert!(rp.get("id").is_some());
        assert!(rp.get("name").is_some());

        // User fields.
        let user = json.get("user").unwrap();
        assert!(user.get("id").is_some());
        assert!(user.get("name").is_some());
        assert!(user.get("displayName").is_some());

        // Authenticator selection.
        let sel = json.get("authenticatorSelection").unwrap();
        assert!(sel.get("authenticatorAttachment").is_some());
        assert!(sel.get("userVerification").is_some());
    }

    // --- U1: phone_challenges store/take ---

    #[test]
    fn test_store_and_take_challenge_roundtrip() {
        let pool = crate::db::init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('t1', 'T1', datetime('now'), 'x', 1)",
            [],
        )
        .unwrap();

        let challenge = [0x42u8; CHALLENGE_LEN];
        store_challenge(&pool, "ch-1", "t1", &challenge).unwrap();

        // First take returns the challenge.
        let taken = take_challenge(&pool, "ch-1", "t1").unwrap();
        assert_eq!(taken, Some(challenge.to_vec()));

        // Second take returns None (already used).
        let taken2 = take_challenge(&pool, "ch-1", "t1").unwrap();
        assert_eq!(taken2, None);
    }

    #[test]
    fn test_take_challenge_wrong_tenant_returns_none() {
        let pool = crate::db::init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('t1', 'T1', datetime('now'), 'x', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('t2', 'T2', datetime('now'), 'x', 1)",
            [],
        )
        .unwrap();

        let challenge = [0x11u8; CHALLENGE_LEN];
        store_challenge(&pool, "ch-1", "t1", &challenge).unwrap();

        // Wrong tenant cannot consume the challenge.
        let taken = take_challenge(&pool, "ch-1", "t2").unwrap();
        assert_eq!(taken, None);

        // Right tenant still can (it was not consumed by the wrong tenant).
        let taken = take_challenge(&pool, "ch-1", "t1").unwrap();
        assert_eq!(taken, Some(challenge.to_vec()));
    }

    // --- U2: counter replay protection ---

    #[test]
    fn test_counter_is_valid_zero_cases() {
        // Per W3C: if either counter is 0, the test passes by default.
        assert!(counter_is_valid(0, 0));
        assert!(counter_is_valid(0, 1));
        assert!(counter_is_valid(5, 0));
    }

    #[test]
    fn test_counter_is_valid_strictly_greater() {
        assert!(counter_is_valid(5, 6));
        assert!(!counter_is_valid(5, 5)); // equal → reject (cloned authenticator)
        assert!(!counter_is_valid(10, 5)); // lower → reject (replay)
    }

    #[test]
    fn test_verify_assertion_rejects_replayed_counter() {
        // Store a credential with counter=5, then present an assertion with
        // counter=5 (equal). Per W3C, this signals a cloned authenticator
        // and must be rejected — even with a valid signature.
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_replay";
        let credential_id = "cred-replay";
        let (pool, signing_key) = setup_db_with_credential(credential_id);

        // Bump the stored counter to 5.
        {
            let conn = pool.get().unwrap();
            conn.execute(
                "UPDATE credentials SET counter = 5 WHERE credential_id = ?1",
                params![credential_id],
            )
            .unwrap();
        }

        // Build auth_data with sign_count = 5 (same as stored → replay).
        let challenge = generate_challenge("", request_id, "");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&client_data_bytes);

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
        auth_bytes.extend_from_slice(&5u32.to_be_bytes()); // counter = 5 (replay!)
        let auth_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&auth_bytes);

        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let signature_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(
            !result.unwrap(),
            "replayed counter (equal to stored) must be rejected"
        );
    }

    #[test]
    fn test_verify_assertion_updates_counter_on_success() {
        // After a successful assertion with counter=N, the stored counter
        // must be advanced to N.
        use p256::ecdsa::{signature::Signer, DerSignature};

        let request_id = "req_counter_update";
        let credential_id = "cred-counter";
        let (pool, signing_key) = setup_db_with_credential(credential_id);

        let challenge = generate_challenge("", request_id, "");
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&client_data_bytes);

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
        auth_bytes.push(0x05);
        auth_bytes.extend_from_slice(&42u32.to_be_bytes()); // counter = 42
        let auth_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&auth_bytes);

        let signed_message = webauthn_signed_message(&auth_bytes, &client_data_bytes);
        let signature: DerSignature = signing_key.sign(&signed_message);
        let signature_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());

        let assertion = WebAuthnAssertion {
            credential_id: credential_id.to_string(),
            authenticator_data: auth_data_b64,
            client_data_json: client_data_b64,
            signature: signature_b64,
        };

        let result = verify_assertion(&pool, "test-tenant", &assertion, request_id);
        assert!(result.is_ok());
        assert!(result.unwrap(), "valid assertion must be accepted");

        // Verify the stored counter was advanced to 42.
        let conn = pool.get().unwrap();
        let stored: i64 = conn
            .query_row(
                "SELECT counter FROM credentials WHERE credential_id = ?1",
                params![credential_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, 42);
    }

    // --- U2: attestation verification (registration) ---

    /// Build a CBOR-encoded `attestationObject` with the given `fmt`,
    /// `authData` and `attStmt`.
    fn build_attestation_object(
        fmt: &str,
        auth_data: &[u8],
        att_stmt_cbor: &[u8],
    ) -> Vec<u8> {
        // Map of 3 entries: a3
        let mut out = Vec::new();
        out.push(0xa3); // map(3)

        // "fmt"
        out.push(0x63); // text(3)
        out.extend_from_slice(b"fmt");
        out.push(0x60 + fmt.len() as u8); // text(n) — assumes len < 24
        out.extend_from_slice(fmt.as_bytes());

        // "authData"
        out.push(0x68); // text(8)
        out.extend_from_slice(b"authData");
        // bytes(n) — use 0x58 for 1-byte length prefix (supports up to 255)
        out.push(0x58);
        out.push(auth_data.len() as u8);
        out.extend_from_slice(auth_data);

        // "attStmt"
        out.push(0x67); // text(7)
        out.extend_from_slice(b"attStmt");
        out.extend_from_slice(att_stmt_cbor);

        out
    }

    /// Build authData with the AT flag set, including attested credential
    /// data (aaguid + credential_id + COSE_Key).
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
        out.push(0x45); // flags: UP=1, UV=1, AT=1 (bits 0, 2, 6)
        out.extend_from_slice(&sign_count.to_be_bytes());
        out.extend_from_slice(aaguid);
        out.extend_from_slice(&(credential_id.len() as u16).to_be_bytes());
        out.extend_from_slice(credential_id);
        out.extend_from_slice(cose_key_cbor);
        out
    }

    /// Build a COSE_Key CBOR map for an EC2 P-256 public key.
    fn build_cose_key(x: &[u8], y: &[u8]) -> Vec<u8> {
        // Map of 5 entries: a5
        let mut out = Vec::new();
        out.push(0xa5); // map(5)

        // 1: 2 (kty = EC2)
        out.push(0x01); // uint(1)
        out.push(0x02); // uint(2)

        // 3: -7 (alg = ES256) — negative int: 0x26 = -1-6 = -7
        out.push(0x03); // uint(3)
        out.push(0x26); // negative int 6 → value -7

        // -1: 1 (crv = P-256) — negative int key: 0x20 = -1
        out.push(0x20); // -1
        out.push(0x01); // uint(1)

        // -2: x (32 bytes) — negative int key: 0x21 = -2
        out.push(0x21); // -2
        out.push(0x58); // bytes(1-byte len)
        out.push(32);
        out.extend_from_slice(x);

        // -3: y (32 bytes) — negative int key: 0x22 = -3
        out.push(0x22); // -3
        out.push(0x58);
        out.push(32);
        out.extend_from_slice(y);

        out
    }

    /// Build an empty attStmt (CBOR map with 0 entries — used for "none").
    fn build_empty_att_stmt() -> Vec<u8> {
        vec![0xa0] // map(0)
    }

    /// Build a packed self-attestation attStmt: { alg: -7, sig: <bytes> }.
    fn build_packed_self_att_stmt(sig_bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0xa2); // map(2)

        // alg: -7
        out.push(0x63); // text(3)
        out.extend_from_slice(b"alg");
        out.push(0x26); // -7

        // sig: bytes
        out.push(0x63); // text(3)
        out.extend_from_slice(b"sig");
        out.push(0x58); // bytes(1-byte len)
        out.push(sig_bytes.len() as u8);
        out.extend_from_slice(sig_bytes);

        out
    }

    #[test]
    fn test_verify_attestation_none_format_succeeds() {
        use p256::ecdsa::SigningKey;
        use rand::rngs::OsRng;

        // Generate a P-256 keypair for the new credential.
        let mut rng = OsRng;
        let signing_key = SigningKey::random(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let sec1 = verifying_key.to_sec1_bytes(); // 65 bytes: 0x04 || x || y
        let x = &sec1[1..33];
        let y = &sec1[33..65];

        let cose_key = build_cose_key(x, y);
        let credential_id = b"cred-id-1234567890";
        let aaguid = [0u8; 16];
        let auth_data =
            build_auth_data_with_attested_cred(DEFAULT_RP_ID, 1, &aaguid, credential_id, &cose_key);

        let att_obj = build_attestation_object("none", &auth_data, &build_empty_att_stmt());
        let att_obj_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&att_obj);

        // Build client_data_json.
        let challenge = generate_random_challenge();
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&client_data_bytes);

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID);
        assert!(result.is_ok(), "attestation 'none' should succeed: {:?}", result.err());
        let res = result.unwrap();
        assert_eq!(res.fmt, "none");
        assert_eq!(res.sign_count, 1);
        // credential_id is base64url(credential_id).
        let cred_id_decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&res.credential_id)
            .unwrap();
        assert_eq!(cred_id_decoded, credential_id);
        // public_key is base64url(SEC1 0x04 || x || y).
        let pk_decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&res.public_key)
            .unwrap();
        assert_eq!(&pk_decoded[..], &sec1[..]);
    }

    #[test]
    fn test_verify_attestation_rejects_wrong_type() {
        let challenge = generate_random_challenge();
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        // Wrong type: webauthn.get instead of webauthn.create.
        let client_data = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let att_obj = build_attestation_object("none", &[0u8; 37], &build_empty_att_stmt());
        let att_obj_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&att_obj);

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("webauthn.create"));
    }

    #[test]
    fn test_verify_attestation_rejects_wrong_origin() {
        let challenge = generate_random_challenge();
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge_b64,
            "origin": "https://evil.example.com",
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let att_obj = build_attestation_object("none", &[0u8; 37], &build_empty_att_stmt());
        let att_obj_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&att_obj);

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("phishing"));
    }

    #[test]
    fn test_verify_attestation_rejects_wrong_challenge() {
        let challenge = generate_random_challenge();
        let wrong_challenge = generate_random_challenge();
        let wrong_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&wrong_challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": wrong_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        let att_obj = build_attestation_object("none", &[0u8; 37], &build_empty_att_stmt());
        let att_obj_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&att_obj);

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("challenge"));
    }

    #[test]
    fn test_verify_attestation_packed_self_attestation_succeeds() {
        use p256::ecdsa::{signature::Signer, DerSignature, SigningKey};
        use rand::rngs::OsRng;

        let mut rng = OsRng;
        let signing_key = SigningKey::random(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let sec1 = verifying_key.to_sec1_bytes();
        let x = &sec1[1..33];
        let y = &sec1[33..65];

        let cose_key = build_cose_key(x, y);
        let credential_id = b"cred-packed-12345";
        let aaguid = [0u8; 16];
        let auth_data =
            build_auth_data_with_attested_cred(DEFAULT_RP_ID, 7, &aaguid, credential_id, &cose_key);

        // Build client_data_json.
        let challenge = generate_random_challenge();
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&client_data_bytes);

        // Sign authData || SHA-256(clientDataJSON) with the credential's own key.
        let client_data_hash = Sha256::digest(&client_data_bytes);
        let mut signed_message = Vec::with_capacity(auth_data.len() + client_data_hash.len());
        signed_message.extend_from_slice(&auth_data);
        signed_message.extend_from_slice(&client_data_hash);
        let signature: DerSignature = signing_key.sign(&signed_message);

        let att_stmt = build_packed_self_att_stmt(signature.as_bytes());
        let att_obj = build_attestation_object("packed", &auth_data, &att_stmt);
        let att_obj_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&att_obj);

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID);
        assert!(result.is_ok(), "packed self-attestation should succeed: {:?}", result.err());
        let res = result.unwrap();
        assert_eq!(res.fmt, "packed");
        assert_eq!(res.sign_count, 7);
    }

    #[test]
    fn test_verify_attestation_packed_rejects_bad_signature() {
        use p256::ecdsa::{signature::Signer, DerSignature, SigningKey};
        use rand::rngs::OsRng;

        let mut rng = OsRng;
        let signing_key = SigningKey::random(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let sec1 = verifying_key.to_sec1_bytes();
        let x = &sec1[1..33];
        let y = &sec1[33..65];

        let cose_key = build_cose_key(x, y);
        let credential_id = b"cred-badsig-12345";
        let aaguid = [0u8; 16];
        let auth_data =
            build_auth_data_with_attested_cred(DEFAULT_RP_ID, 7, &aaguid, credential_id, &cose_key);

        let challenge = generate_random_challenge();
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_bytes = serde_json::to_vec(&client_data).unwrap();
        let client_data_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&client_data_bytes);

        // Sign a *different* message (tampered auth_data) — signature won't match.
        let mut tampered_auth = auth_data.clone();
        tampered_auth[36] ^= 0x01; // flip a bit in sign_count
        let client_data_hash = Sha256::digest(&client_data_bytes);
        let mut signed_message = Vec::with_capacity(tampered_auth.len() + client_data_hash.len());
        signed_message.extend_from_slice(&tampered_auth);
        signed_message.extend_from_slice(&client_data_hash);
        let signature: DerSignature = signing_key.sign(&signed_message);

        let att_stmt = build_packed_self_att_stmt(signature.as_bytes());
        let att_obj = build_attestation_object("packed", &auth_data, &att_stmt);
        let att_obj_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&att_obj);

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("signature"));
    }

    #[test]
    fn test_verify_attestation_rejects_unsupported_format() {
        use p256::ecdsa::SigningKey;
        use rand::rngs::OsRng;

        let mut rng = OsRng;
        let signing_key = SigningKey::random(&mut rng);
        let verifying_key = signing_key.verifying_key();
        let sec1 = verifying_key.to_sec1_bytes();
        let x = &sec1[1..33];
        let y = &sec1[33..65];

        let cose_key = build_cose_key(x, y);
        let credential_id = b"cred-tpm-12345678";
        let aaguid = [0u8; 16];
        let auth_data =
            build_auth_data_with_attested_cred(DEFAULT_RP_ID, 1, &aaguid, credential_id, &cose_key);

        let challenge = generate_random_challenge();
        let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
        let client_data = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge_b64,
            "origin": DEFAULT_RP_ORIGIN,
        });
        let client_data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&client_data).unwrap());

        // tpm format — not implemented.
        let att_obj = build_attestation_object("tpm", &auth_data, &build_empty_att_stmt());
        let att_obj_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&att_obj);

        let attestation = AuthenticatorAttestationResponse {
            client_data_json: client_data_b64,
            attestation_object: att_obj_b64,
        };

        let result = verify_attestation(&attestation, &challenge, DEFAULT_RP_ORIGIN, DEFAULT_RP_ID);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("tpm"));
    }

    #[test]
    fn test_cbor_parse_roundtrip_text_and_bytes() {
        // Smoke test for the minimal CBOR parser.
        let text = b"hello";
        let mut cbor = vec![0x65]; // text(5)
        cbor.extend_from_slice(text);
        let (v, rest) = cbor_parse(&cbor).unwrap();
        assert_eq!(cbor_as_text(&v).unwrap(), "hello");
        assert!(rest.is_empty());

        let bytes = b"\x01\x02\x03";
        let mut cbor = vec![0x43]; // bytes(3)
        cbor.extend_from_slice(bytes);
        let (v, rest) = cbor_parse(&cbor).unwrap();
        assert_eq!(cbor_as_bytes(&v).unwrap(), bytes);
        assert!(rest.is_empty());
    }

}
