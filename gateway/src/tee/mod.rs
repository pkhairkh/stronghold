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

#[cfg(feature = "sev-snp")]
pub mod sev_snp;

#[cfg(not(feature = "sev-snp"))]
pub mod no_sev;

#[cfg(feature = "sev-snp")]
pub use sev_snp::*;

#[cfg(not(feature = "sev-snp"))]
pub use no_sev::*;
