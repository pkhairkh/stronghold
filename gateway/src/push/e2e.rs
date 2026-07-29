//! End-to-end encryption for ntfy push payloads.
//!
//! Uses hybrid KEM: X25519 + ML-KEM-768 → HKDF-256 → AES-256-GCM.
//!
//! The phone generates both keypairs at enrollment (via WASM in the browser
//! using @noble/post-quantum). The public halves are uploaded to the gateway.
//! Each push: gateway encapsulates with both → derives AES key → encrypts payload.
//!
//! Implemented in: W5-T6 (encrypt + decrypt + base64 encode/decode round-trip)
//! Tested by: gateway/src/push/e2e.rs (unit tests)

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::Result;
use base64::Engine;

use crate::crypto::hybrid_kem::{self, PushKeys};

/// Encrypt a push payload using the phone's hybrid public keys.
pub fn encrypt(
    plaintext: &[u8],
    phone_x25519_pub: &[u8],
    phone_mlkem_pub: &[u8],
) -> Result<EncryptedPayload> {
    // Encapsulate shared secret
    let (encapsulated, shared_secret) =
        hybrid_kem::encapsulate(phone_x25519_pub, phone_mlkem_pub)?;

    // Derive AES key
    let aes_key = hybrid_kem::derive_aes_key(&shared_secret, b"stronghold-push-v1");

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

/// Decrypt a push payload using the phone's hybrid secret keys.
///
/// This is the phone-side mirror of [`encrypt`]: the phone decapsulates
/// the shared secret using its `PushKeys` (X25519 + ML-KEM-768 secret
/// halves), derives the same AES-256 key via HKDF-256, then decrypts
/// the ciphertext with AES-256-GCM.
///
/// Returns the recovered plaintext, or an error if the keys don't match
/// the encapsulation, the nonce is malformed, or the ciphertext has been
/// tampered with (AES-GCM auth tag fails).
pub fn decrypt(payload: &EncryptedPayload, phone_keys: &PushKeys) -> Result<Vec<u8>> {
    // Decapsulate the shared secret with the phone's secret keys.
    let shared_secret = hybrid_kem::decapsulate(phone_keys, &payload.encapsulated)?;

    // Derive the same AES key (same info string as encrypt).
    let aes_key = hybrid_kem::derive_aes_key(&shared_secret, b"stronghold-push-v1");

    if payload.nonce.len() != 12 {
        return Err(anyhow::anyhow!(
            "nonce is {} bytes, expected 12",
            payload.nonce.len()
        ));
    }
    let nonce = Nonce::from_slice(&payload.nonce);

    let cipher = Aes256Gcm::new_from_slice(&aes_key)
        .map_err(|e| anyhow::anyhow!("aes-gcm key init: {:?}", e))?;
    let plaintext = cipher
        .decrypt(nonce, payload.ciphertext.as_ref())
        .map_err(|e| anyhow::anyhow!("aes-gcm decrypt (wrong keys or tampered ciphertext): {:?}", e))?;

    Ok(plaintext)
}

/// An encrypted push payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedPayload {
    pub encapsulated: hybrid_kem::EncapsulatedSecret,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Encode an encrypted payload as base64 for the ntfy message body.
pub fn encode(payload: &EncryptedPayload) -> String {
    let json = serde_json::to_string(payload).unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(json)
}

/// Decode a base64-encoded encrypted payload (the inverse of [`encode`]).
///
/// Used by the phone to recover the `EncryptedPayload` from the ntfy
/// message body before calling [`decrypt`].
pub fn decode(b64: &str) -> Result<EncryptedPayload> {
    let json = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| anyhow::anyhow!("base64 decode: {:?}", e))?;
    let payload: EncryptedPayload = serde_json::from_slice(&json)
        .map_err(|e| anyhow::anyhow!("json decode: {:?}", e))?;
    Ok(payload)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: gateway encrypts with phone's public halves → phone
    /// decrypts with its PushKeys → recovered plaintext matches original.
    #[test]
    fn test_encrypt_decrypt_round_trip() {
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let plaintext = b"approve session sess_01H8XK2P3F4YTB5NJCZ6Q7R8S9";

        let encrypted = encrypt(plaintext, &x_pub, &m_pub).unwrap();
        let recovered = decrypt(&encrypted, &phone).unwrap();

        assert_eq!(recovered.as_slice(), plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_empty_payload() {
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();

        let encrypted = encrypt(b"", &x_pub, &m_pub).unwrap();
        let recovered = decrypt(&encrypted, &phone).unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn test_encrypt_decrypt_large_payload() {
        // 64 KiB plaintext — exercises AES-GCM chunking.
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let plaintext: Vec<u8> = (0..65_536).map(|i| (i % 256) as u8).collect();

        let encrypted = encrypt(&plaintext, &x_pub, &m_pub).unwrap();
        let recovered = decrypt(&encrypted, &phone).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn test_decrypt_with_wrong_phone_keys_fails() {
        // Encrypt for one phone, try to decrypt with a different phone's keys.
        let phone_a = PushKeys::generate();
        let (xa_pub, ma_pub) = phone_a.public_halves();
        let phone_b = PushKeys::generate();

        let plaintext = b"secret anomaly alert";
        let encrypted = encrypt(plaintext, &xa_pub, &ma_pub).unwrap();

        let result = decrypt(&encrypted, &phone_b);
        assert!(
            result.is_err(),
            "decrypt with wrong phone keys must fail"
        );
    }

    #[test]
    fn test_decrypt_with_tampered_ciphertext_fails() {
        // AES-GCM provides authenticated encryption — any modification to
        // the ciphertext must cause decryption to fail.
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let plaintext = b"approve";

        let mut encrypted = encrypt(plaintext, &x_pub, &m_pub).unwrap();
        // Flip a bit in the ciphertext.
        encrypted.ciphertext[0] ^= 0xff;

        let result = decrypt(&encrypted, &phone);
        assert!(
            result.is_err(),
            "tampered ciphertext must fail AES-GCM auth"
        );
    }

    #[test]
    fn test_decrypt_with_tampered_nonce_fails() {
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let plaintext = b"approve";

        let mut encrypted = encrypt(plaintext, &x_pub, &m_pub).unwrap();
        // Flip a bit in the nonce.
        encrypted.nonce[0] ^= 0xff;

        let result = decrypt(&encrypted, &phone);
        assert!(result.is_err(), "tampered nonce must fail decryption");
    }

    #[test]
    fn test_decrypt_with_wrong_size_nonce_fails() {
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let plaintext = b"approve";

        let mut encrypted = encrypt(plaintext, &x_pub, &m_pub).unwrap();
        // Truncate the nonce to 11 bytes.
        encrypted.nonce.truncate(11);

        let result = decrypt(&encrypted, &phone);
        assert!(result.is_err(), "wrong-size nonce must fail");
    }

    #[test]
    fn test_encrypt_produces_different_ciphertexts_for_same_plaintext() {
        // Each encryption uses a fresh ephemeral X25519 keypair + fresh nonce,
        // so two encryptions of the same plaintext must differ.
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let plaintext = b"same plaintext";

        let e1 = encrypt(plaintext, &x_pub, &m_pub).unwrap();
        let e2 = encrypt(plaintext, &x_pub, &m_pub).unwrap();

        assert_ne!(e1.ciphertext, e2.ciphertext, "ciphertexts must differ");
        assert_ne!(e1.nonce, e2.nonce, "nonces must differ");
        assert_ne!(
            e1.encapsulated.x25519_ciphertext,
            e2.encapsulated.x25519_ciphertext,
            "ephemeral X25519 pubs must differ"
        );

        // Both must still decrypt to the same plaintext.
        assert_eq!(decrypt(&e1, &phone).unwrap().as_slice(), plaintext);
        assert_eq!(decrypt(&e2, &phone).unwrap().as_slice(), plaintext);
    }

    #[test]
    fn test_encode_decode_round_trip() {
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let plaintext = b"hello ntfy";

        let encrypted = encrypt(plaintext, &x_pub, &m_pub).unwrap();
        let b64 = encode(&encrypted);
        let decoded = decode(&b64).unwrap();

        assert_eq!(decoded.ciphertext, encrypted.ciphertext);
        assert_eq!(decoded.nonce, encrypted.nonce);
        assert_eq!(
            decoded.encapsulated.x25519_ciphertext,
            encrypted.encapsulated.x25519_ciphertext
        );
        assert_eq!(
            decoded.encapsulated.mlkem_ciphertext,
            encrypted.encapsulated.mlkem_ciphertext
        );

        // And the decoded payload must still decrypt.
        assert_eq!(decrypt(&decoded, &phone).unwrap().as_slice(), plaintext);
    }

    #[test]
    fn test_encode_produces_valid_base64() {
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let encrypted = encrypt(b"x", &x_pub, &m_pub).unwrap();
        let b64 = encode(&encrypted);

        // Must be valid standard base64 (no URL-safe chars, padding intact).
        assert!(
            b64.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "encode must produce standard base64"
        );
    }

    #[test]
    fn test_decode_rejects_invalid_base64() {
        let result = decode("!!! not base64 !!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_rejects_valid_base64_but_invalid_json() {
        // Valid base64, but the decoded bytes aren't JSON.
        let bogus = base64::engine::general_purpose::STANDARD.encode(b"not json");
        let result = decode(&bogus);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypted_payload_does_not_contain_plaintext() {
        // Security property: the encrypted payload (ciphertext + nonce +
        // encapsulated secret) must NOT contain any substring of the
        // plaintext. This is the AES-GCM confidentiality guarantee.
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();
        let plaintext = b"sensitive-command-rm-rf-sensitive-marker";
        let encrypted = encrypt(plaintext, &x_pub, &m_pub).unwrap();

        // The plaintext must not appear anywhere in the encrypted payload.
        let combined: Vec<u8> = encrypted
            .ciphertext
            .iter()
            .chain(encrypted.nonce.iter())
            .chain(encrypted.encapsulated.x25519_ciphertext.iter())
            .chain(encrypted.encapsulated.mlkem_ciphertext.iter())
            .copied()
            .collect();

        // Search for any 4-byte window of the plaintext in the combined bytes.
        for window_start in 0..=(plaintext.len().saturating_sub(4)) {
            let window = &plaintext[window_start..window_start + 4];
            assert!(
                !contains_subslice(&combined, window),
                "plaintext substring {:?} found in encrypted payload — confidentiality broken",
                window
            );
        }
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack
            .windows(needle.len())
            .any(|w| w == needle)
    }
}
