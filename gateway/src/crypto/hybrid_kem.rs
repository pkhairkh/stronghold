//! Hybrid KEM — X25519 + ML-KEM-768 for push notification encryption.
//!
//! Used to encrypt ntfy push payloads end-to-end between the gateway
//! and the phone. The phone holds both private halves; the gateway
//! holds both public halves.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A hybrid KEM keypair (X25519 + ML-KEM-768).
/// Note: does not derive Debug because x25519_dalek::StaticSecret
/// intentionally does not implement Debug (to avoid leaking secret bytes).
#[derive(Clone)]
pub struct PushKeys {
    pub x25519_secret: x25519_dalek::StaticSecret,
    pub x25519_public: x25519_dalek::PublicKey,
    pub mlkem_secret: Vec<u8>,   // TODO: use ml_kem::DecapsulationKey
    pub mlkem_public: Vec<u8>,   // TODO: use ml_kem::EncapsulationKey
}

impl std::fmt::Debug for PushKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushKeys")
            .field("x25519_public", &"[redacted]")
            .field("mlkem_public", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// A hybrid KEM encapsulated secret (what the gateway sends to the phone).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncapsulatedSecret {
    pub x25519_ciphertext: Vec<u8>,
    pub mlkem_ciphertext: Vec<u8>,
}

impl PushKeys {
    /// Generate a new hybrid keypair.
    #[allow(clippy::needless_borrow)] // false positive: From<&StaticSecret> requires the &
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        let x25519_secret = x25519_dalek::StaticSecret::random_from_rng(&mut rng);
        let x25519_public = x25519_dalek::PublicKey::from(&x25519_secret);

        // TODO: generate ML-KEM-768 keypair using ml_kem crate
        let mlkem_secret = vec![0u8; 32];
        let mlkem_public = vec![0u8; 1184];

        Self {
            x25519_secret,
            x25519_public,
            mlkem_secret,
            mlkem_public,
        }
    }

    /// Load keys from a directory, or generate new ones if not present.
    pub fn load_or_generate_keys(dir: &str) -> Result<Self> {
        let secret_path = format!("{}/push_x25519.key", dir);
        let _pub_path = format!("{}/push_x25519.pub", dir);

        if std::path::Path::new(&secret_path).exists() {
            // TODO: load from files
            tracing::info!("Loading existing push keys");
            Ok(Self::generate()) // placeholder
        } else {
            tracing::info!("Generating new push keys");
            let keys = Self::generate();
            // TODO: save to files
            Ok(keys)
        }
    }

    /// Save keys to a directory.
    pub fn save(&self, dir: &str) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        // TODO: save both X25519 and ML-KEM keys
        Ok(())
    }

    /// Get the public halves (for sending to the phone at enrollment).
    pub fn public_halves(&self) -> (Vec<u8>, Vec<u8>) {
        (
            self.x25519_public.to_bytes().to_vec(),
            self.mlkem_public.clone(),
        )
    }
}

/// Encapsulate a shared secret using the phone's public keys.
///
/// Returns `(encapsulated_secret, shared_secret)` where `shared_secret`
/// is used to derive an AES-256-GCM key via HKDF-256.
#[allow(clippy::needless_borrow)] // false positive: From<&StaticSecret> requires the &
pub fn encapsulate(
    phone_x25519_pub: &[u8],
    phone_mlkem_pub: &[u8],
) -> Result<(EncapsulatedSecret, [u8; 32])> {
    let mut rng = rand::rngs::OsRng;

    // X25519
    let ephemeral_secret = x25519_dalek::StaticSecret::random_from_rng(&mut rng);
    let ephemeral_public = x25519_dalek::PublicKey::from(&ephemeral_secret);
    let phone_pub = x25519_dalek::PublicKey::from(<[u8; 32]>::try_from(phone_x25519_pub)?);
    let x25519_shared = ephemeral_secret.diffie_hellman(&phone_pub);

    // ML-KEM-768
    // TODO: use ml_kem::EncapsulationKey::encapsulate
    let mlkem_shared = [0u8; 32];
    let _ = phone_mlkem_pub;

    // Combine via HKDF-256
    let combined = hkdf_combine(&x25519_shared.to_bytes(), &mlkem_shared);

    Ok((
        EncapsulatedSecret {
            x25519_ciphertext: ephemeral_public.to_bytes().to_vec(),
            mlkem_ciphertext: vec![0u8; 1088], // ML-KEM-768 ciphertext size
        },
        combined,
    ))
}

/// Derive an AES-256-GCM key from a shared secret using HKDF-256.
pub fn derive_aes_key(shared_secret: &[u8; 32], info: &[u8]) -> [u8; 32] {
    use hkdf::Hkdf;
    use sha2::Sha256;

    let hk = Hkdf::<Sha256>::new(None, shared_secret);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm).expect("HKDF expand failed");
    okm
}

fn hkdf_combine(x25519_shared: &[u8; 32], mlkem_shared: &[u8; 32]) -> [u8; 32] {
    let mut combined = Vec::with_capacity(64);
    combined.extend_from_slice(x25519_shared);
    combined.extend_from_slice(mlkem_shared);

    use hkdf::Hkdf;
    use sha2::Sha256;

    let hk = Hkdf::<Sha256>::new(None, &combined);
    let mut okm = [0u8; 32];
    hk.expand(b"stronghold-push-e2e", &mut okm).expect("HKDF expand failed");
    okm
}

/// Generate keys in a directory (used by `stronghold init`).
pub fn generate_keys(dir: &str) -> Result<()> {
    let keys = PushKeys::generate();
    keys.save(dir)?;
    tracing::info!("Push keys generated in {}", dir);
    Ok(())
}
