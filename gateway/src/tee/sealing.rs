//! Key sealing — encrypt keys to a measurement-derived key.
//!
//! This module provides the cryptographic primitives used by both the
//! real SEV-SNP path (`sev_snp.rs`) and the no-SEV fallback (`no_sev.rs`).
//!
//! ## Real SEV-SNP (production)
//!
//! On a real SEV-SNP guest, the sealing key is derived from the AMD Secure
//! Processor via `Firmware::get_derived_key()` (a 32-byte hardware-derived
//! key mixed with the launch measurement). That key is then used to
//! AES-256-GCM encrypt the audit / push keys at rest.
//!
//! ## Stub / dev (this module)
//!
//! Because the dev box has no `/dev/sev-guest`, the same HKDF + AES-GCM
//! construction is exercised here with a key derived from the measurement
//! *string* via HKDF-SHA256. This lets us unit-test the seal/unseal
//! round-trip and tamper-detection logic without any SEV-SNP hardware.
//!
//! The wire format for sealed keys is:
//! ```text
//!   [12-byte AES-GCM nonce] [ciphertext + 16-byte GCM tag]
//! ```
//!
//! Sealed keys are *bound* to the measurement: changing the measurement
//! (e.g., by modifying the gateway binary) changes the derived key, which
//! causes AES-GCM authentication to fail on unseal.

use anyhow::Result;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use aes_gcm::aead::Aead;
use hkdf::Hkdf;
use sha2::Sha256;

/// AES-256-GCM nonce length (96 bits, per NIST SP 800-38D).
pub const NONCE_LEN: usize = 12;

/// AES-256 key length.
pub const KEY_LEN: usize = 32;

/// HKDF info string domain-separating Stronghold key sealing from other
/// consumers of HKDF-SHA256 in the same process.
const SEAL_INFO: &[u8] = b"stronghold-seal-keys-v1";

/// Derive a 32-byte AES-256 key from a measurement string using HKDF-SHA256.
///
/// On real SEV-SNP hardware this string is the hex-encoded 48-byte launch
/// measurement reported by the firmware. On the dev stub it is whatever
/// `current_measurement()` returned (a fixed placeholder or `n/a`).
///
/// The same measurement always derives the same key (deterministic), and a
/// different measurement derives a different key (collision-resistant under
/// SHA-256). This is what makes sealed keys measurement-bound.
pub fn derive_sealing_key(measurement: &str) -> [u8; KEY_LEN] {
    // HKDF-256 with empty salt (the measurement is already high-entropy on
    // real hardware — 48 bytes of SHA-384 output) and a domain-separation
    // info string.
    let hk = Hkdf::<Sha256>::new(None, measurement.as_bytes());
    let mut okm = [0u8; KEY_LEN];
    // expand() returns Ok(()) only if the requested length is <= 255*HashLen.
    // 32 bytes is well under that limit, so unwrap is safe here.
    hk.expand(SEAL_INFO, &mut okm)
        .expect("HKDF-SHA256 expand of 32 bytes cannot fail");
    okm
}

/// Seal (encrypt) `plaintext` with a 32-byte key.
///
/// Returns `nonce || ciphertext+tag`. The nonce is randomly generated per
/// call (CSPRNG) and prepended to the output so the caller doesn't have to
/// track it separately.
pub fn seal_with_key(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new(key.into());
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("aes-gcm seal failed: {:?}", e))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Unseal (decrypt) data produced by [`seal_with_key`].
///
/// Expects `nonce || ciphertext+tag`. Returns the plaintext on success, or
/// an error if the key is wrong, the nonce was tampered with, or the GCM
/// authentication tag does not verify (which is what makes sealed keys
/// measurement-bound).
pub fn unseal_with_key(key: &[u8; KEY_LEN], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < NONCE_LEN {
        return Err(anyhow::anyhow!(
            "sealed blob too short: {} bytes, need >= {}",
            sealed.len(),
            NONCE_LEN
        ));
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("aes-gcm unseal failed (wrong measurement or tampered?): {:?}", e))?;
    Ok(plaintext)
}

/// Convenience: seal `plaintext` bound to a measurement string.
///
/// Combines [`derive_sealing_key`] and [`seal_with_key`].
pub fn seal_with_measurement(measurement: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let key = derive_sealing_key(measurement);
    seal_with_key(&key, plaintext)
}

/// Convenience: unseal a blob that was sealed to a measurement string.
///
/// The caller MUST supply the *same* measurement used at seal time. Any
/// difference (binary modified, kernel upgraded, etc.) causes the GCM tag
/// verification to fail.
pub fn unseal_with_measurement(measurement: &str, sealed: &[u8]) -> Result<Vec<u8>> {
    let key = derive_sealing_key(measurement);
    unseal_with_key(&key, sealed)
}

// bring OsRng::fill_bytes into scope without polluting the public API
use rand::RngCore;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- W7-T3 / W7-T4: key sealing round-trip ---

    #[test]
    fn test_seal_unseal_round_trip() {
        let measurement = "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let plaintext = b"super-secret-audit-signing-key-bytes";
        let sealed = seal_with_measurement(measurement, plaintext).unwrap();
        assert_ne!(&sealed[..], &plaintext[..]);
        let unsealed = unseal_with_measurement(measurement, &sealed).unwrap();
        assert_eq!(&unsealed[..], &plaintext[..]);
    }

    #[test]
    fn test_seal_unseal_empty_input() {
        let measurement = "sha256:0";
        let sealed = seal_with_measurement(measurement, b"").unwrap();
        assert_eq!(sealed.len(), NONCE_LEN + 16); // nonce + GCM tag for empty plaintext
        let unsealed = unseal_with_measurement(measurement, &sealed).unwrap();
        assert!(unsealed.is_empty());
    }

    #[test]
    fn test_seal_unseal_large_input() {
        let measurement = "sha256:large-test";
        let plaintext = vec![0x42u8; 64 * 1024];
        let sealed = seal_with_measurement(measurement, &plaintext).unwrap();
        let unsealed = unseal_with_measurement(measurement, &sealed).unwrap();
        assert_eq!(unsealed, plaintext);
    }

    #[test]
    fn test_seal_produces_different_ciphertext_each_call() {
        // Random nonce → sealing the same plaintext twice must yield different
        // sealed blobs (semantic security).
        let measurement = "sha256:nonce-uniqueness";
        let plaintext = b"identical-plaintext";
        let s1 = seal_with_measurement(measurement, plaintext).unwrap();
        let s2 = seal_with_measurement(measurement, plaintext).unwrap();
        assert_ne!(s1, s2);
        // Both must still unseal correctly.
        assert_eq!(unseal_with_measurement(measurement, &s1).unwrap(), plaintext);
        assert_eq!(unseal_with_measurement(measurement, &s2).unwrap(), plaintext);
    }

    #[test]
    fn test_sealed_blob_includes_nonce_prefix() {
        let measurement = "sha256:format-check";
        let plaintext = b"1234567890";
        let sealed = seal_with_measurement(measurement, plaintext).unwrap();
        // Format: 12-byte nonce + ciphertext + 16-byte GCM tag.
        // ciphertext length == plaintext length for AES-GCM.
        assert_eq!(
            sealed.len(),
            NONCE_LEN + plaintext.len() + 16
        );
    }

    // --- W7-T3 / W7-T4: wrong measurement fails to unseal ---

    #[test]
    fn test_unseal_fails_with_wrong_measurement() {
        let seal_measurement = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let unseal_measurement = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let plaintext = b"audit-keys-ed25519-secret-32-bytes!!!"; // 37 bytes
        let sealed = seal_with_measurement(seal_measurement, plaintext).unwrap();

        // Wrong measurement → derived key differs → GCM tag fails to verify.
        let result = unseal_with_measurement(unseal_measurement, &sealed);
        assert!(result.is_err(), "unseal with wrong measurement must fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unseal failed") || err.contains("wrong measurement") || err.contains("tampered"),
            "error message should hint at the cause, got: {}",
            err
        );
    }

    #[test]
    fn test_unseal_fails_when_measurement_differs_by_one_char() {
        // Even a single-character change in the measurement changes the
        // derived key and breaks unseal. This is what protects against
        // binary tampering (the launch measurement is a hash of the binary).
        let m1 = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let m2 = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
        let plaintext = b"sensitive-key-material";
        let sealed = seal_with_measurement(m1, plaintext).unwrap();
        assert!(unseal_with_measurement(m2, &sealed).is_err());
    }

    #[test]
    fn test_unseal_fails_on_tampered_ciphertext() {
        let measurement = "sha256:tamper-test";
        let plaintext = b"hello, world";
        let mut sealed = seal_with_measurement(measurement, plaintext).unwrap();

        // Flip a bit in the ciphertext body (after the nonce).
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;

        assert!(unseal_with_measurement(measurement, &sealed).is_err());
    }

    #[test]
    fn test_unseal_fails_on_tampered_nonce() {
        let measurement = "sha256:nonce-tamper";
        let plaintext = b"hello, world";
        let mut sealed = seal_with_measurement(measurement, plaintext).unwrap();

        // Flip a bit in the nonce (first 12 bytes).
        sealed[0] ^= 0x01;

        // Decryption either fails outright or yields different plaintext that
        // fails the GCM tag check. Either way, unseal must error.
        assert!(unseal_with_measurement(measurement, &sealed).is_err());
    }

    #[test]
    fn test_unseal_rejects_short_input() {
        let measurement = "sha256:short-input";
        // Less than NONCE_LEN bytes → reject before attempting decryption.
        let result = unseal_with_measurement(measurement, b"too-short");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    // --- W7-T3: key derivation properties ---

    #[test]
    fn test_derive_key_is_deterministic() {
        let m = "sha256:deterministic-test-measurement";
        let k1 = derive_sealing_key(m);
        let k2 = derive_sealing_key(m);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), KEY_LEN);
    }

    #[test]
    fn test_derive_key_differs_per_measurement() {
        let m1 = "sha256:measurement-one";
        let m2 = "sha256:measurement-two";
        let k1 = derive_sealing_key(m1);
        let k2 = derive_sealing_key(m2);
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_derive_key_is_domain_separated() {
        // The HKDF info string b"stronghold-seal-keys-v1" must be in use;
        // verify by checking that an ad-hoc HKDF without the info string
        // produces a different key.
        let m = "sha256:domain-sep-test";
        let with_info = derive_sealing_key(m);

        let hk_no_info = Hkdf::<Sha256>::new(None, m.as_bytes());
        let mut without_info = [0u8; KEY_LEN];
        hk_no_info.expand(b"", &mut without_info).unwrap();

        assert_ne!(with_info, without_info);
    }

    // --- W7-T3: seal_with_key / unseal_with_key direct API ---

    #[test]
    fn test_seal_unseal_with_explicit_key() {
        let key = [0x42u8; KEY_LEN];
        let plaintext = b"explicit-key-test";
        let sealed = seal_with_key(&key, plaintext).unwrap();
        let unsealed = unseal_with_key(&key, &sealed).unwrap();
        assert_eq!(&unsealed[..], &plaintext[..]);
    }

    #[test]
    fn test_seal_unseal_with_wrong_key_fails() {
        let key1 = [0x42u8; KEY_LEN];
        let key2 = [0x99u8; KEY_LEN];
        let plaintext = b"explicit-key-test";
        let sealed = seal_with_key(&key1, plaintext).unwrap();
        assert!(unseal_with_key(&key2, &sealed).is_err());
    }
}
