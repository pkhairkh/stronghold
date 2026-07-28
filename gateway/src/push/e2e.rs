//! End-to-end encryption for ntfy push payloads.
//!
//! Uses hybrid KEM: X25519 + ML-KEM-768 → HKDF-256 → AES-256-GCM.
//!
//! The phone generates both keypairs at enrollment (via WASM in the browser
//! using @noble/post-quantum). The public halves are uploaded to the gateway.
//! Each push: gateway encapsulates with both → derives AES key → encrypts payload.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::Result;
use base64::Engine;

/// Encrypt a push payload using the phone's hybrid public keys.
pub fn encrypt(
    plaintext: &[u8],
    phone_x25519_pub: &[u8],
    phone_mlkem_pub: &[u8],
) -> Result<EncryptedPayload> {
    // Encapsulate shared secret
    let (encapsulated, shared_secret) =
        crate::crypto::hybrid_kem::encapsulate(phone_x25519_pub, phone_mlkem_pub)?;

    // Derive AES key
    let aes_key = crate::crypto::hybrid_kem::derive_aes_key(&shared_secret, b"stronghold-push-v1");

    // Generate nonce (12 bytes)
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt
    let cipher = Aes256Gcm::new_from_slice(&aes_key)
        .map_err(|e| anyhow::anyhow!("aes-gcm key init: {:?}", e))?;
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {:?}", e))?;

    Ok(EncryptedPayload {
        encapsulated,
        nonce: nonce_bytes.to_vec(),
        ciphertext,
    })
}

/// An encrypted push payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedPayload {
    pub encapsulated: crate::crypto::hybrid_kem::EncapsulatedSecret,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Encode an encrypted payload as base64 for the ntfy message body.
pub fn encode(payload: &EncryptedPayload) -> String {
    let json = serde_json::to_string(payload).unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(json)
}
