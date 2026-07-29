//! Cryptography — post-quantum hybrid everywhere.
//!
//! Layers:
//! 1. Transport: TLS 1.3 + X25519Kyber768Draft00 (rustls)
//! 2. Audit signatures: Ed25519 + ML-DSA-65 (dual-signed)
//! 3. Push encryption: X25519 + ML-KEM-768 (hybrid KEM → AES-256-GCM)
//! 4. WebAuthn: classical for now (FIDO PQC authenticators not deployed yet)

pub mod hybrid_kem;
pub mod hybrid_sig;
pub mod tls;
pub mod vault;
pub mod webauthn;
