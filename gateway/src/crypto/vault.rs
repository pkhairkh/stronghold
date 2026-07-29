//! Credential vault — encrypted storage for agent secrets (SSH keys, API tokens, etc.).
//!
//! Implemented in: K1
//!
//! Uses AES-256-GCM with per-tenant keys derived from the audit Ed25519
//! secret key + tenant_id via HKDF-256. The tenant key is derived in
//! memory, never stored.

use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use aes_gcm::aead::Aead;
use anyhow::Result;
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

use crate::crypto::hybrid_sig::AuditKeys;

/// AES-256-GCM key length (32 bytes).
const KEY_LEN: usize = 32;
/// AES-256-GCM nonce length (12 bytes).
const NONCE_LEN: usize = 12;
/// HKDF info string for credential vault key derivation.
const VAULT_INFO: &[u8] = b"stronghold-credential-vault-v1";

/// Derive a per-tenant AES-256 key from the audit Ed25519 secret key + tenant_id.
///
/// The key is derived via HKDF-256:
/// - IKM = Ed25519 secret key bytes (32 bytes)
/// - salt = tenant_id (UTF-8 bytes)
/// - info = "stronghold-credential-vault-v1"
///
/// The tenant key is never stored — it's derived on demand.
pub fn derive_tenant_key(tenant_id: &str, audit_keys: &AuditKeys) -> [u8; KEY_LEN] {
    let ed_secret = audit_keys.ed25519_secret_bytes();
    let hk = Hkdf::<Sha256>::new(Some(tenant_id.as_bytes()), &ed_secret);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(VAULT_INFO, &mut okm).expect("HKDF expand failed");
    okm
}

/// Encrypt a plaintext credential value with the tenant key.
///
/// Returns `(ciphertext, nonce)` where both are raw bytes.
/// The nonce is 12 bytes, generated randomly per encryption.
pub fn encrypt(plaintext: &[u8], tenant_key: &[u8; KEY_LEN]) -> Result<(Vec<u8>, Vec<u8>)> {
    let cipher = Aes256Gcm::new_from_slice(tenant_key)
        .map_err(|e| anyhow::anyhow!("AES key init failed: {:?}", e))?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("AES-GCM encrypt failed: {:?}", e))?;

    Ok((ciphertext, nonce_bytes.to_vec()))
}

/// Decrypt a ciphertext with the tenant key and nonce.
///
/// Returns the plaintext bytes.
pub fn decrypt(ciphertext: &[u8], nonce: &[u8], tenant_key: &[u8; KEY_LEN]) -> Result<Vec<u8>> {
    if nonce.len() != NONCE_LEN {
        return Err(anyhow::anyhow!(
            "nonce is {} bytes, expected {}",
            nonce.len(),
            NONCE_LEN
        ));
    }

    let cipher = Aes256Gcm::new_from_slice(tenant_key)
        .map_err(|e| anyhow::anyhow!("AES key init failed: {:?}", e))?;

    let nonce = Nonce::from_slice(nonce);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("AES-GCM decrypt failed: {:?}", e))?;

    Ok(plaintext)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_round_trip() {
        let audit_keys = AuditKeys::generate();
        let tenant_key = derive_tenant_key("tenant_test", &audit_keys);
        let plaintext = b"github_pat_abc123_secret_token";

        let (ciphertext, nonce) = encrypt(plaintext, &tenant_key).unwrap();
        let decrypted = decrypt(&ciphertext, &nonce, &tenant_key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_decrypt_rejects_wrong_key() {
        let audit_keys1 = AuditKeys::generate();
        let audit_keys2 = AuditKeys::generate();
        let key1 = derive_tenant_key("tenant_a", &audit_keys1);
        let key2 = derive_tenant_key("tenant_a", &audit_keys2);

        let plaintext = b"secret_value";
        let (ciphertext, nonce) = encrypt(plaintext, &key1).unwrap();

        // Decrypting with a different key should fail
        assert!(decrypt(&ciphertext, &nonce, &key2).is_err());
    }

    #[test]
    fn test_decrypt_rejects_wrong_nonce() {
        let audit_keys = AuditKeys::generate();
        let tenant_key = derive_tenant_key("tenant_test", &audit_keys);
        let plaintext = b"secret_value";

        let (ciphertext, _) = encrypt(plaintext, &tenant_key).unwrap();
        let wrong_nonce = vec![0u8; NONCE_LEN];

        assert!(decrypt(&ciphertext, &wrong_nonce, &tenant_key).is_err());
    }

    #[test]
    fn test_different_tenants_get_different_keys() {
        let audit_keys = AuditKeys::generate();
        let key_a = derive_tenant_key("tenant_a", &audit_keys);
        let key_b = derive_tenant_key("tenant_b", &audit_keys);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_same_tenant_gets_same_key() {
        let audit_keys = AuditKeys::generate();
        let key1 = derive_tenant_key("tenant_a", &audit_keys);
        let key2 = derive_tenant_key("tenant_a", &audit_keys);
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_different_audit_keys_produce_different_tenant_keys() {
        let audit_keys1 = AuditKeys::generate();
        let audit_keys2 = AuditKeys::generate();
        let key1 = derive_tenant_key("tenant_a", &audit_keys1);
        let key2 = derive_tenant_key("tenant_a", &audit_keys2);
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_ciphertext_differs_from_plaintext() {
        let audit_keys = AuditKeys::generate();
        let tenant_key = derive_tenant_key("tenant_test", &audit_keys);
        let plaintext = b"hello_world_secret";

        let (ciphertext, _) = encrypt(plaintext, &tenant_key).unwrap();

        // Ciphertext should not contain the plaintext
        assert!(!ciphertext.windows(plaintext.len()).any(|w| w == plaintext));
    }

    #[test]
    fn test_each_encryption_produces_different_ciphertext() {
        let audit_keys = AuditKeys::generate();
        let tenant_key = derive_tenant_key("tenant_test", &audit_keys);
        let plaintext = b"same_plaintext";

        let (ct1, _) = encrypt(plaintext, &tenant_key).unwrap();
        let (ct2, _) = encrypt(plaintext, &tenant_key).unwrap();

        // Different random nonces → different ciphertexts
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn test_decrypt_rejects_tampered_ciphertext() {
        let audit_keys = AuditKeys::generate();
        let tenant_key = derive_tenant_key("tenant_test", &audit_keys);
        let plaintext = b"secret_value";

        let (mut ciphertext, nonce) = encrypt(plaintext, &tenant_key).unwrap();

        // Flip a bit in the ciphertext
        ciphertext[0] ^= 0x01;

        assert!(decrypt(&ciphertext, &nonce, &tenant_key).is_err());
    }

    #[test]
    fn test_decrypt_rejects_short_nonce() {
        let audit_keys = AuditKeys::generate();
        let tenant_key = derive_tenant_key("tenant_test", &audit_keys);
        let plaintext = b"secret";

        let (ciphertext, _) = encrypt(plaintext, &tenant_key).unwrap();
        let short_nonce = vec![0u8; 8]; // wrong size

        assert!(decrypt(&ciphertext, &short_nonce, &tenant_key).is_err());
    }
}
