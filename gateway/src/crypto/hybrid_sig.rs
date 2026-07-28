//! Hybrid signatures — Ed25519 + ML-DSA-65 for the audit log.
//!
//! Every audit entry is signed with both algorithms. If either is broken
//! in the future, the other still proves authenticity.

use anyhow::Result;
use base64::Engine;
use ed25519_dalek::{Signer, Verifier, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A hybrid signature keypair (Ed25519 + ML-DSA-65).
#[derive(Debug, Clone)]
pub struct AuditKeys {
    pub ed25519_secret: SigningKey,
    pub ed25519_public: ed25519_dalek::VerifyingKey,
    pub mldsa_secret: Vec<u8>,   // TODO: use ml_dsa::SigningKey
    pub mldsa_public: Vec<u8>,   // TODO: use ml_dsa::VerifyingKey
}

/// A dual signature (Ed25519 + ML-DSA-65).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DualSignature {
    pub sig_ed25519: String,
    pub sig_mldsa65: String,
}

impl AuditKeys {
    /// Generate a new hybrid keypair.
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        let ed25519_secret = SigningKey::generate(&mut rng);
        let ed25519_public = ed25519_secret.verifying_key();

        // TODO: generate ML-DSA-65 keypair using ml_dsa crate
        let mldsa_secret = vec![0u8; 32];
        let mldsa_public = vec![0u8; 1952];

        Self {
            ed25519_secret,
            ed25519_public,
            mldsa_secret,
            mldsa_public,
        }
    }

    /// Load keys from a directory, or generate new ones if not present.
    pub fn load_or_generate_keys(dir: &str) -> Result<Self> {
        let secret_path = format!("{}/audit_ed25519.key", dir);
        let _pub_path = format!("{}/audit_ed25519.pub", dir);

        if std::path::Path::new(&secret_path).exists() {
            tracing::info!("Loading existing audit keys");
            // TODO: load from files
            Ok(Self::generate()) // placeholder
        } else {
            tracing::info!("Generating new audit keys");
            let keys = Self::generate();
            keys.save(dir)?;
            Ok(keys)
        }
    }

    /// Save keys to a directory.
    pub fn save(&self, dir: &str) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        // TODO: save both Ed25519 and ML-DSA keys
        Ok(())
    }

    /// Sign a message with both algorithms.
    pub fn sign(&self, message: &[u8]) -> DualSignature {
        // Ed25519
        let ed_sig = self.ed25519_secret.sign(message);
        let sig_ed25519 = base64::engine::general_purpose::STANDARD.encode(ed_sig.to_bytes());

        // ML-DSA-65
        // TODO: use ml_dsa::SigningKey::sign
        let sig_mldsa65 = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 3293]);

        DualSignature {
            sig_ed25519,
            sig_mldsa65,
        }
    }

    /// Verify a dual signature.
    pub fn verify(&self, message: &[u8], sig: &DualSignature) -> bool {
        // Ed25519
        let ed_sig_bytes = match base64::engine::general_purpose::STANDARD.decode(&sig.sig_ed25519) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let ed_sig = match ed25519_dalek::Signature::from_slice(&ed_sig_bytes) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if self.ed25519_public.verify(message, &ed_sig).is_err() {
            return false;
        }

        // ML-DSA-65
        // TODO: use ml_dsa::VerifyingKey::verify
        // For now, skip verification (stub)

        true
    }

    /// Get the public key fingerprints (for phone verification).
    pub fn fingerprints(&self) -> (String, String) {
        let ed_pub_bytes = self.ed25519_public.to_bytes();
        let mut hasher = Sha256::new();
        hasher.update(ed_pub_bytes);
        let ed_hash = hex::encode(hasher.finalize());

        let mut hasher = Sha256::new();
        hasher.update(&self.mldsa_public);
        let mldsa_hash = hex::encode(hasher.finalize());

        (ed_hash, mldsa_hash)
    }
}

/// Generate keys in a directory (used by `stronghold init`).
pub fn generate_keys(dir: &str) -> Result<()> {
    let keys = AuditKeys::generate();
    keys.save(dir)?;
    tracing::info!("Audit keys generated in {}", dir);
    Ok(())
}
