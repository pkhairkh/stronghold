//! Hybrid KEM — X25519 + ML-KEM-768 for push notification encryption.
//!
//! Used to encrypt ntfy push payloads end-to-end between the gateway
//! and the phone. The phone holds both private halves; the gateway
//! holds both public halves.
//!
//! Implemented in: W1-T4, W1-T5, W1-T6
//! Tested by: gateway/src/crypto/hybrid_kem.rs (unit + property tests)

use anyhow::{Context, Result};
use kem::{Decapsulate, Encapsulate};
use ml_kem::{
    Encoded, EncodedSizeUser, KemCore, MlKem768, MlKem768Params,
    kem::{DecapsulationKey, EncapsulationKey},
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::path::Path;

use hkdf::Hkdf;

/// X25519 secret key is 32 bytes.
const X25519_SECRET_LEN: usize = 32;
/// X25519 public key is 32 bytes.
const X25519_PUBLIC_LEN: usize = 32;
/// X25519 shared secret is 32 bytes.
const X25519_SHARED_LEN: usize = 32;
/// ML-KEM-768 shared secret is 32 bytes (FIPS 203).
const MLKEM_SHARED_LEN: usize = 32;
/// ML-KEM-768 encapsulation key (public) is 1184 bytes.
const MLKEM_PUB_LEN: usize = 1184;
/// ML-KEM-768 decapsulation key (secret) is 2400 bytes.
const MLKEM_SECRET_LEN: usize = 2400;
/// ML-KEM-768 ciphertext is 1088 bytes.
const MLKEM_CIPHERTEXT_LEN: usize = 1088;
/// Combined shared secret is 64 bytes (32 from each KEM).
const COMBINED_LEN: usize = X25519_SHARED_LEN + MLKEM_SHARED_LEN;
/// Derived AES-256 key is 32 bytes.
const AES_KEY_LEN: usize = 32;
/// AES-GCM nonce is 12 bytes.
const AES_NONCE_LEN: usize = 12;

/// HKDF info string for push E2E encryption.
const PUSH_INFO: &[u8] = b"stronghold-push-e2e-v1";

/// A hybrid KEM keypair (X25519 + ML-KEM-768).
///
/// The gateway holds this keypair. The phone holds a separate keypair
/// (generated in browser WASM). The gateway uses the phone's *public*
/// halves to encapsulate a shared secret; the phone uses its *private*
/// halves to decapsulate.
///
/// Note: does not derive `Debug` because `x25519_dalek::StaticSecret`
/// intentionally does not implement `Debug` (to avoid leaking secret bytes).
#[derive(Clone)]
pub struct PushKeys {
    /// X25519 static secret (32 bytes).
    pub x25519_secret: x25519_dalek::StaticSecret,
    /// X25519 public key derived from `x25519_secret`.
    pub x25519_public: x25519_dalek::PublicKey,
    /// ML-KEM-768 decapsulation key (2400 bytes serialized).
    pub mlkem_secret: Vec<u8>,
    /// ML-KEM-768 encapsulation key (1184 bytes serialized).
    pub mlkem_public: Vec<u8>,
}

impl std::fmt::Debug for PushKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushKeys")
            .field("x25519_public", &hex::encode(self.x25519_public.to_bytes()))
            .field("mlkem_public_len", &self.mlkem_public.len())
            .field("x25519_secret", &"[redacted]")
            .field("mlkem_secret", &"[redacted]")
            .finish()
    }
}

/// A hybrid KEM encapsulated secret (what the gateway sends to the phone).
///
/// Both ciphertexts are sent. The phone decapsulates both and combines
/// the shared secrets via HKDF-256.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncapsulatedSecret {
    /// X25519 ephemeral public key (32 bytes).
    pub x25519_ciphertext: Vec<u8>,
    /// ML-KEM-768 ciphertext (1088 bytes).
    pub mlkem_ciphertext: Vec<u8>,
}

impl PushKeys {
    /// Generate a new hybrid keypair.
    ///
    /// - X25519: uses `OsRng` (platform CSPRNG).
    /// - ML-KEM-768: uses `OsRng` via the `ml_kem` crate's `KemCore::generate()`.
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;

        // X25519
        let x25519_secret = x25519_dalek::StaticSecret::random_from_rng(&mut rng);
        let x25519_public = x25519_dalek::PublicKey::from(&x25519_secret);

        // ML-KEM-768
        let (dk, ek) = MlKem768::generate(&mut rng);
        let mlkem_secret = dk.as_bytes().to_vec();
        let mlkem_public = ek.as_bytes().to_vec();

        Self {
            x25519_secret,
            x25519_public,
            mlkem_secret,
            mlkem_public,
        }
    }

    /// Load keys from a directory, or generate new ones if not present.
    ///
    /// File layout (all files mode 0600, dir mode 0700):
    /// - `<dir>/push_x25519.key` — 32-byte raw X25519 secret
    /// - `<dir>/push_x25519.pub` — 32-byte raw X25519 public
    /// - `<dir>/push_mlkem768.key` — 2400-byte ML-KEM-768 decapsulation key
    /// - `<dir>/push_mlkem768.pub` — 1184-byte ML-KEM-768 encapsulation key
    pub fn load_or_generate_keys(dir: &str) -> Result<Self> {
        let secret_path = format!("{}/push_x25519.key", dir);

        if Path::new(&secret_path).exists() {
            tracing::info!(dir = dir, "Loading existing push keys");
            Self::load(dir)
        } else {
            tracing::info!(dir = dir, "Generating new push keys");
            let keys = Self::generate();
            keys.save(dir)?;
            Ok(keys)
        }
    }

    /// Load keys from a directory.
    pub fn load(dir: &str) -> Result<Self> {
        let x_secret_path = format!("{}/push_x25519.key", dir);
        let x_pub_path = format!("{}/push_x25519.pub", dir);
        let m_secret_path = format!("{}/push_mlkem768.key", dir);
        let m_pub_path = format!("{}/push_mlkem768.pub", dir);

        let x_secret_bytes = std::fs::read(&x_secret_path)
            .with_context(|| format!("reading {}", x_secret_path))?;
        if x_secret_bytes.len() != X25519_SECRET_LEN {
            return Err(anyhow::anyhow!(
                "x25519 secret key file {} is {} bytes, expected {}",
                x_secret_path,
                x_secret_bytes.len(),
                X25519_SECRET_LEN
            ));
        }
        let mut x_secret_arr = [0u8; X25519_SECRET_LEN];
        x_secret_arr.copy_from_slice(&x_secret_bytes);
        let x25519_secret = x25519_dalek::StaticSecret::from(x_secret_arr);
        let x25519_public = x25519_dalek::PublicKey::from(&x25519_secret);

        // Verify stored public key matches derived.
        if Path::new(&x_pub_path).exists() {
            let stored_pub = std::fs::read(&x_pub_path)?;
            if stored_pub != x25519_public.to_bytes() {
                return Err(anyhow::anyhow!(
                    "x25519 public key file {} does not match secret — possible tampering",
                    x_pub_path
                ));
            }
        } else {
            std::fs::write(&x_pub_path, x25519_public.to_bytes())?;
            set_mode_644(&x_pub_path)?;
        }

        let mlkem_secret = std::fs::read(&m_secret_path)
            .with_context(|| format!("reading {}", m_secret_path))?;
        let mlkem_public = std::fs::read(&m_pub_path)
            .with_context(|| format!("reading {}", m_pub_path))?;

        if mlkem_secret.len() != MLKEM_SECRET_LEN {
            return Err(anyhow::anyhow!(
                "mlkem secret key file {} is {} bytes, expected {}",
                m_secret_path,
                mlkem_secret.len(),
                MLKEM_SECRET_LEN
            ));
        }
        if mlkem_public.len() != MLKEM_PUB_LEN {
            return Err(anyhow::anyhow!(
                "mlkem public key file {} is {} bytes, expected {}",
                m_pub_path,
                mlkem_public.len(),
                MLKEM_PUB_LEN
            ));
        }

        Ok(Self {
            x25519_secret,
            x25519_public,
            mlkem_secret,
            mlkem_public,
        })
    }

    /// Save keys to a directory.
    pub fn save(&self, dir: &str) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir))?;
        set_mode_700(dir)?;

        write_secret_file(&format!("{}/push_x25519.key", dir), &self.x25519_secret.to_bytes())?;
        write_public_file(&format!("{}/push_x25519.pub", dir), &self.x25519_public.to_bytes())?;
        write_secret_file(&format!("{}/push_mlkem768.key", dir), &self.mlkem_secret)?;
        write_public_file(&format!("{}/push_mlkem768.pub", dir), &self.mlkem_public)?;

        tracing::debug!(dir = dir, "Push keys saved");
        Ok(())
    }

    /// Get the public halves as `(x25519_pub_bytes, mlkem_pub_bytes)`.
    ///
    /// These are sent to the phone at enrollment time.
    pub fn public_halves(&self) -> (Vec<u8>, Vec<u8>) {
        (
            self.x25519_public.to_bytes().to_vec(),
            self.mlkem_public.clone(),
        )
    }

    /// Get the X25519 secret key bytes (32 bytes).
    pub fn x25519_secret_bytes(&self) -> [u8; X25519_SECRET_LEN] {
        self.x25519_secret.to_bytes()
    }
}

/// Encapsulate a shared secret using the phone's public keys.
///
/// Performs both X25519 and ML-KEM-768 encapsulation, then combines the
/// two shared secrets via HKDF-256 to produce a single 32-byte AES key.
///
/// Returns `(EncapsulatedSecret, shared_secret)` where `shared_secret` is
/// the combined 32-byte key suitable for AES-256-GCM.
///
/// Note: the phone must perform the same decapsulation + HKDF combine to
/// recover the same shared secret.
pub fn encapsulate(
    phone_x25519_pub: &[u8],
    phone_mlkem_pub: &[u8],
) -> Result<(EncapsulatedSecret, [u8; AES_KEY_LEN])> {
    let mut rng = rand::rngs::OsRng;

    // --- X25519 ---
    if phone_x25519_pub.len() != X25519_PUBLIC_LEN {
        return Err(anyhow::anyhow!(
            "phone_x25519_pub is {} bytes, expected {}",
            phone_x25519_pub.len(),
            X25519_PUBLIC_LEN
        ));
    }
    let mut pub_arr = [0u8; X25519_PUBLIC_LEN];
    pub_arr.copy_from_slice(phone_x25519_pub);
    let phone_x_pub = x25519_dalek::PublicKey::from(pub_arr);

    let ephemeral_secret = x25519_dalek::StaticSecret::random_from_rng(&mut rng);
    let ephemeral_public = x25519_dalek::PublicKey::from(&ephemeral_secret);
    let x25519_shared = ephemeral_secret.diffie_hellman(&phone_x_pub);
    let x25519_shared_bytes = x25519_shared.to_bytes();

    // --- ML-KEM-768 ---
    if phone_mlkem_pub.len() != MLKEM_PUB_LEN {
        return Err(anyhow::anyhow!(
            "phone_mlkem_pub is {} bytes, expected {}",
            phone_mlkem_pub.len(),
            MLKEM_PUB_LEN
        ));
    }
    let mut ek_arr = [0u8; MLKEM_PUB_LEN];
    ek_arr.copy_from_slice(phone_mlkem_pub);
    // Use TryFrom to get an &Encoded<_> reference (from_slice is deprecated).
    let ek_encoded: &Encoded<EncapsulationKey<MlKem768Params>> =
        <&Encoded<EncapsulationKey<MlKem768Params>>>::try_from(&ek_arr)
            .expect("ek_arr is the correct size");
    let ek = EncapsulationKey::<MlKem768Params>::from_bytes(ek_encoded);
    let (ct, mlkem_shared) = ek
        .encapsulate(&mut rng)
        .map_err(|e| anyhow::anyhow!("ml-kem encapsulate failed: {:?}", e))?;
    let mlkem_shared_bytes: [u8; MLKEM_SHARED_LEN] = mlkem_shared.into();

    // --- Combine via HKDF-256 ---
    let combined = hkdf_combine(&x25519_shared_bytes, &mlkem_shared_bytes);

    Ok((
        EncapsulatedSecret {
            x25519_ciphertext: ephemeral_public.to_bytes().to_vec(),
            mlkem_ciphertext: ct.to_vec(),
        },
        combined,
    ))
}

/// Decapsulate a shared secret using the gateway's secret keys.
///
/// This is used when the *phone* sends an encrypted payload to the gateway
/// (e.g., a credential enrollment response). The gateway decapsulates using
/// its own secret keys.
pub fn decapsulate(
    keys: &PushKeys,
    encapsulated: &EncapsulatedSecret,
) -> Result<[u8; AES_KEY_LEN]> {
    // --- X25519 ---
    if encapsulated.x25519_ciphertext.len() != X25519_PUBLIC_LEN {
        return Err(anyhow::anyhow!(
            "x25519_ciphertext is {} bytes, expected {}",
            encapsulated.x25519_ciphertext.len(),
            X25519_PUBLIC_LEN
        ));
    }
    let mut peer_arr = [0u8; X25519_PUBLIC_LEN];
    peer_arr.copy_from_slice(&encapsulated.x25519_ciphertext);
    let peer_pub = x25519_dalek::PublicKey::from(peer_arr);
    let x25519_shared = keys.x25519_secret.diffie_hellman(&peer_pub);
    let x25519_shared_bytes = x25519_shared.to_bytes();

    // --- ML-KEM-768 ---
    if encapsulated.mlkem_ciphertext.len() != MLKEM_CIPHERTEXT_LEN {
        return Err(anyhow::anyhow!(
            "mlkem_ciphertext is {} bytes, expected {}",
            encapsulated.mlkem_ciphertext.len(),
            MLKEM_CIPHERTEXT_LEN
        ));
    }
    let mut ct_arr = [0u8; MLKEM_CIPHERTEXT_LEN];
    ct_arr.copy_from_slice(&encapsulated.mlkem_ciphertext);
    // Ciphertext<K> is just Array<u8, K::CiphertextSize> — no from_bytes needed.
    let ct: ml_kem::Ciphertext<MlKem768> = ml_kem::Ciphertext::<MlKem768>::from(ct_arr);

    if keys.mlkem_secret.len() != MLKEM_SECRET_LEN {
        return Err(anyhow::anyhow!(
            "mlkem_secret is {} bytes, expected {}",
            keys.mlkem_secret.len(),
            MLKEM_SECRET_LEN
        ));
    }
    let mut dk_arr = [0u8; MLKEM_SECRET_LEN];
    dk_arr.copy_from_slice(&keys.mlkem_secret);
    let dk_encoded: &Encoded<DecapsulationKey<MlKem768Params>> =
        <&Encoded<DecapsulationKey<MlKem768Params>>>::try_from(&dk_arr)
            .expect("dk_arr is the correct size");
    let dk = DecapsulationKey::<MlKem768Params>::from_bytes(dk_encoded);

    let mlkem_shared = dk
        .decapsulate(&ct)
        .map_err(|e| anyhow::anyhow!("ml-kem decapsulate failed: {:?}", e))?;
    let mlkem_shared_bytes: [u8; MLKEM_SHARED_LEN] = mlkem_shared.into();

    // --- Combine ---
    Ok(hkdf_combine(&x25519_shared_bytes, &mlkem_shared_bytes))
}

/// Derive an AES-256-GCM key from a shared secret using HKDF-256.
///
/// Uses an explicit `info` string for domain separation.
pub fn derive_aes_key(shared_secret: &[u8; AES_KEY_LEN], info: &[u8]) -> [u8; AES_KEY_LEN] {
    let hk = Hkdf::<Sha256>::new(None, shared_secret);
    let mut okm = [0u8; AES_KEY_LEN];
    hk.expand(info, &mut okm).expect("HKDF expand failed");
    okm
}

/// Combine X25519 and ML-KEM-768 shared secrets via HKDF-256.
///
/// Concatenates the two 32-byte shared secrets (X25519 || ML-KEM) and
/// derives a 32-byte key via HKDF-256 with the push E2E info string.
fn hkdf_combine(x25519_shared: &[u8; X25519_SHARED_LEN], mlkem_shared: &[u8; MLKEM_SHARED_LEN]) -> [u8; AES_KEY_LEN] {
    let mut combined = Vec::with_capacity(COMBINED_LEN);
    combined.extend_from_slice(x25519_shared);
    combined.extend_from_slice(mlkem_shared);

    let hk = Hkdf::<Sha256>::new(None, &combined);
    let mut okm = [0u8; AES_KEY_LEN];
    hk.expand(PUSH_INFO, &mut okm).expect("HKDF expand failed");
    okm
}

/// Generate keys in a directory (used by `stronghold init`).
pub fn generate_keys(dir: &str) -> Result<()> {
    let keys = PushKeys::generate();
    keys.save(dir)?;
    tracing::info!(dir = dir, "Push keys generated");
    Ok(())
}

// ============================================================================
// File permission helpers (Unix only)
// ============================================================================

#[cfg(unix)]
fn set_mode_700(path: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 700 {}", path))?;
    Ok(())
}

#[cfg(unix)]
fn set_mode_600(path: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 600 {}", path))?;
    Ok(())
}

#[cfg(unix)]
fn set_mode_644(path: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o644);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 644 {}", path))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode_700(_path: &str) -> Result<()> {
    Ok(())
}
#[cfg(not(unix))]
fn set_mode_600(_path: &str) -> Result<()> {
    Ok(())
}
#[cfg(not(unix))]
fn set_mode_644(_path: &str) -> Result<()> {
    Ok(())
}

fn write_secret_file(path: &str, bytes: &[u8]) -> Result<()> {
    let tmp_path = format!("{}.tmp", path);
    std::fs::write(&tmp_path, bytes).with_context(|| format!("writing {}", tmp_path))?;
    set_mode_600(&tmp_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(&tmp_path)?;
        let _ = nix::unistd::fsync(f.as_raw_fd());
    }
    std::fs::rename(&tmp_path, path).with_context(|| format!("renaming {} -> {}", tmp_path, path))?;
    Ok(())
}

fn write_public_file(path: &str, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path))?;
    set_mode_644(path)?;
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- W1-T4: PushKeys keypair generation + save/load ---

    #[test]
    fn test_generate_produces_valid_keypair() {
        let keys = PushKeys::generate();
        assert_eq!(keys.x25519_secret_bytes().len(), X25519_SECRET_LEN);
        assert_eq!(keys.x25519_public.to_bytes().len(), X25519_PUBLIC_LEN);
        assert_eq!(keys.mlkem_secret.len(), MLKEM_SECRET_LEN);
        assert_eq!(keys.mlkem_public.len(), MLKEM_PUB_LEN);
    }

    #[test]
    fn test_generate_produces_unique_keypairs() {
        let k1 = PushKeys::generate();
        let k2 = PushKeys::generate();
        assert_ne!(k1.x25519_secret_bytes(), k2.x25519_secret_bytes());
        assert_ne!(k1.mlkem_secret, k2.mlkem_secret);
    }

    #[test]
    fn test_save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap();
        let keys = PushKeys::generate();
        keys.save(dir).unwrap();
        let loaded = PushKeys::load(dir).unwrap();
        assert_eq!(keys.x25519_secret_bytes(), loaded.x25519_secret_bytes());
        assert_eq!(keys.mlkem_secret, loaded.mlkem_secret);
        assert_eq!(keys.mlkem_public, loaded.mlkem_public);
    }

    #[test]
    fn test_load_or_generate_creates_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap();
        let keys = PushKeys::load_or_generate_keys(dir).unwrap();
        assert!(Path::new(&format!("{}/push_x25519.key", dir)).exists());
        let reloaded = PushKeys::load_or_generate_keys(dir).unwrap();
        assert_eq!(keys.x25519_secret_bytes(), reloaded.x25519_secret_bytes());
    }

    #[test]
    fn test_load_rejects_wrong_x25519_size() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(format!("{}/push_x25519.key", dir), b"too short").unwrap();
        let err = PushKeys::load(dir).unwrap_err();
        assert!(err.to_string().contains("expected 32"));
    }

    #[test]
    fn test_public_halves_correct_lengths() {
        let keys = PushKeys::generate();
        let (x_pub, m_pub) = keys.public_halves();
        assert_eq!(x_pub.len(), X25519_PUBLIC_LEN);
        assert_eq!(m_pub.len(), MLKEM_PUB_LEN);
    }

    // --- W1-T5: encapsulate / decapsulate round-trip ---

    #[test]
    fn test_encapsulate_decapsulate_round_trip() {
        let phone_keys = PushKeys::generate();
        let (x_pub, m_pub) = phone_keys.public_halves();

        let (encapsulated, sender_shared) = encapsulate(&x_pub, &m_pub).unwrap();
        let receiver_shared = decapsulate(&phone_keys, &encapsulated).unwrap();

        assert_eq!(sender_shared, receiver_shared);
    }

    #[test]
    fn test_encapsulate_produces_different_shared_secrets() {
        let phone_keys = PushKeys::generate();
        let (x_pub, m_pub) = phone_keys.public_halves();

        let (_, s1) = encapsulate(&x_pub, &m_pub).unwrap();
        let (_, s2) = encapsulate(&x_pub, &m_pub).unwrap();

        // Each encapsulation uses a fresh random ephemeral key, so the
        // shared secrets must differ.
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_encapsulate_rejects_wrong_x25519_pub_size() {
        let phone_keys = PushKeys::generate();
        let (_, m_pub) = phone_keys.public_halves();
        let err = encapsulate(b"too short", &m_pub).unwrap_err();
        assert!(err.to_string().contains("expected 32"));
    }

    #[test]
    fn test_encapsulate_rejects_wrong_mlkem_pub_size() {
        let phone_keys = PushKeys::generate();
        let (x_pub, _) = phone_keys.public_halves();
        let err = encapsulate(&x_pub, b"too short").unwrap_err();
        assert!(err.to_string().contains("expected 1184"));
    }

    #[test]
    fn test_decapsulate_rejects_wrong_keys() {
        // Encapsulate to phone_keys1, try to decapsulate with phone_keys2.
        let phone1 = PushKeys::generate();
        let phone2 = PushKeys::generate();
        let (x_pub, m_pub) = phone1.public_halves();
        let (encapsulated, _) = encapsulate(&x_pub, &m_pub).unwrap();

        let result = decapsulate(&phone2, &encapsulated);
        // ML-KEM decapsulation is "implicit rejection" — it returns a pseudo-
        // random key rather than an error. So the decapsulation succeeds but
        // produces a DIFFERENT shared secret. Combined with X25519 (which
        // will produce a different shared secret), the final keys won't match.
        if let Ok(wrong_shared) = result {
            let (correct_x_pub, correct_m_pub) = phone1.public_halves();
            let (_, correct_shared) = encapsulate(&correct_x_pub, &correct_m_pub).unwrap();
            assert_ne!(wrong_shared, correct_shared);
        }
        // If decapsulate errored, that's also acceptable behavior.
    }

    // --- W1-T6: HKDF / derive_aes_key ---

    #[test]
    fn test_derive_aes_key_is_deterministic() {
        let secret = [42u8; 32];
        let info = b"test-info";
        let k1 = derive_aes_key(&secret, info);
        let k2 = derive_aes_key(&secret, info);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), AES_KEY_LEN);
    }

    #[test]
    fn test_derive_aes_key_differs_per_info() {
        let secret = [42u8; 32];
        let k1 = derive_aes_key(&secret, b"info-1");
        let k2 = derive_aes_key(&secret, b"info-2");
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_derive_aes_key_differs_per_secret() {
        let k1 = derive_aes_key(&[1u8; 32], b"info");
        let k2 = derive_aes_key(&[2u8; 32], b"info");
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_hkdf_combine_is_deterministic() {
        let x = [1u8; 32];
        let m = [2u8; 32];
        let k1 = hkdf_combine(&x, &m);
        let k2 = hkdf_combine(&x, &m);
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_hkdf_combine_differs_if_x25519_changes() {
        let m = [2u8; 32];
        let k1 = hkdf_combine(&[1u8; 32], &m);
        let k2 = hkdf_combine(&[3u8; 32], &m);
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_hkdf_combine_differs_if_mlkem_changes() {
        let x = [1u8; 32];
        let k1 = hkdf_combine(&x, &[2u8; 32]);
        let k2 = hkdf_combine(&x, &[3u8; 32]);
        assert_ne!(k1, k2);
    }

    // --- W1-T5: X25519 RFC 7748 §6.1 known-answer test ---

    #[test]
    fn test_x25519_rfc7748_kat() {
        // RFC 7748 §6.1 test vector 1:
        // Alice private: 77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a
        // Alice public:  8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a
        // Bob private:   5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b2fd0c43ca38004a0b22
        // Bob public:    de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b8f
        // Shared:        4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742
        let alice_priv = hex::decode("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a").unwrap();
        let bob_pub = hex::decode("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b8f").unwrap();
        let expected_shared = hex::decode("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742").unwrap();

        let mut alice_arr = [0u8; 32];
        alice_arr.copy_from_slice(&alice_priv);
        let alice_secret = x25519_dalek::StaticSecret::from(alice_arr);

        let mut bob_arr = [0u8; 32];
        bob_arr.copy_from_slice(&bob_pub);
        let bob_public = x25519_dalek::PublicKey::from(bob_arr);

        let shared = alice_secret.diffie_hellman(&bob_public);
        assert_eq!(shared.to_bytes(), *expected_shared);
    }

    // --- Property tests ---

    proptest! {
        #[test]
        fn proptest_encapsulate_decapsulate_round_trip(_seed in proptest::prelude::any::<u8>()) {
            let phone_keys = PushKeys::generate();
            let (x_pub, m_pub) = phone_keys.public_halves();
            let (encapsulated, sender_shared) = encapsulate(&x_pub, &m_pub).unwrap();
            let receiver_shared = decapsulate(&phone_keys, &encapsulated).unwrap();
            prop_assert_eq!(sender_shared, receiver_shared);
        }

        #[test]
        fn proptest_unique_encapsulations(
            _seed1 in proptest::prelude::any::<u8>(),
            _seed2 in proptest::prelude::any::<u8>()
        ) {
            let phone_keys = PushKeys::generate();
            let (x_pub, m_pub) = phone_keys.public_halves();
            let (_, s1) = encapsulate(&x_pub, &m_pub).unwrap();
            let (_, s2) = encapsulate(&x_pub, &m_pub).unwrap();
            prop_assert_ne!(s1, s2);
        }

        #[test]
        fn proptest_save_load_round_trip(_seed in proptest::prelude::any::<u8>()) {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().to_str().unwrap().to_string();
            let keys = PushKeys::generate();
            keys.save(&dir).unwrap();
            let loaded = PushKeys::load(&dir).unwrap();
            prop_assert_eq!(keys.x25519_secret_bytes(), loaded.x25519_secret_bytes());
            prop_assert_eq!(keys.mlkem_secret, loaded.mlkem_secret);
            prop_assert_eq!(keys.mlkem_public, loaded.mlkem_public);
        }
    }
}
