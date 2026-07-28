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

use crate::routes::phone::WebAuthnAssertion;
use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

/// Verify a WebAuthn assertion.
///
/// Checks:
/// 1. Signature is valid for the registered credential
/// 2. Challenge matches `sha256(cmd_hash + request_id)` — binds signature to this approval
/// 3. Origin matches gateway's origin (anti-phishing)
/// 4. `user_verified == true` (biometric/PIN was used)
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
        "Verifying WebAuthn assertion (stub)"
    );

    // TODO: implement actual WebAuthn verification using webauthn-rs
    // For now, return true (stub — must be replaced before production!)
    Ok(true)
}

/// Generate a WebAuthn challenge for a new approval request.
///
/// The challenge is `sha256(cmd_hash + request_id + session_scope_hash)`,
/// binding this approval to a specific command/request/scope triple.
pub fn generate_challenge(cmd_hash: &str, request_id: &str, scope_hash: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(cmd_hash.as_bytes());
    hasher.update(request_id.as_bytes());
    hasher.update(scope_hash.as_bytes());
    hasher.finalize().to_vec()
}
