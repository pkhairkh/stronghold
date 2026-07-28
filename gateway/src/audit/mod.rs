//! Audit log — dual-signed (Ed25519 + ML-DSA-65), hash-chained, SEV-SNP attested.
//!
//! Every audit entry is:
//! - Append-only
//! - Hash-chained (includes SHA-256 of previous entry)
//! - Dual-signed with Ed25519 + ML-DSA-65
//! - Includes SEV-SNP attestation report hash (when running in TEE)
//! - Stored in per-tenant SQLite database
//! - Verifiable offline (no gateway access needed)

pub mod log;
pub mod verify;
pub mod export;
