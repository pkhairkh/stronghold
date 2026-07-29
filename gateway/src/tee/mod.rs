//! TEE / SEV-SNP — attestation driver.
//!
//! Behind the `sev-snp` cargo feature flag (default-on in v1).
//! When compiled without SEV-SNP (`--features no-sev-snp`), all functions
//! return stubs and `verify_sev_snp_available()` returns Ok(()).
//!
//! SEV-SNP provides:
//! - Encrypted RAM (hypervisor sees ciphertext only)
//! - Integrity protection (hypervisor cannot modify memory without detection)
//! - Attestation (phone can verify gateway runs on genuine SEV-SNP hardware)
//! - Launch measurement binding (audit keys sealed to measurement)
//!
//! ## Module layout
//!
//! - [`sealing`] — HKDF + AES-GCM key-sealing primitives, compiled under
//!   both feature flags. Used by both the real SEV-SNP path and the dev
//!   fallback. Tested on the dev box (no hardware required).
//! - `sev_snp` — real SEV-SNP attestation + key sealing (uses the `sev`
//!   crate; falls back to a stub report when `/dev/sev-guest` is absent).
//! - `no_sev` — non-TEE stub. `sev_snp_active: false`, seal/unseal are
//!   pass-throughs.

pub mod sealing;

#[cfg(feature = "sev-snp")]
pub mod sev_snp;

#[cfg(not(feature = "sev-snp"))]
pub mod no_sev;

#[cfg(feature = "sev-snp")]
pub use sev_snp::*;

#[cfg(not(feature = "sev-snp"))]
pub use no_sev::*;
