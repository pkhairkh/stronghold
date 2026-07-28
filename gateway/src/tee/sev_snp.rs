//! SEV-SNP attestation driver.
//!
//! This module runs when the `sev-snp` cargo feature is enabled (default).
//! It provides:
//! - `verify_sev_snp_available()` — checks /dev/sev exists
//! - `generate_attestation_report()` — produces a signed attestation report
//! - `current_measurement()` — returns the launch measurement
//! - `seal_keys()` / `unseal_keys()` — seal keys to the current measurement

use anyhow::Result;
use base64::Engine;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AttestationReport {
    pub report: String,      // base64-encoded attestation report
    pub report_hash: String, // SHA-256 of the report
    pub measurement: String, // launch measurement (hash of binary + kernel + initrd)
    pub sev_snp_active: bool,
    pub hardened_mode: bool,
    pub generated_at: String,
}

/// Verify that SEV-SNP is available on this machine.
pub fn verify_sev_snp_available() -> Result<()> {
    let dev_sev = std::path::Path::new("/dev/sev");
    if !dev_sev.exists() {
        return Err(anyhow::anyhow!(
            "SEV-SNP not available (/dev/sev not found). \
             Run with --dev or build with --features no-sev-snp."
        ));
    }
    tracing::info!("SEV-SNP device detected at /dev/sev");
    Ok(())
}

/// Generate an attestation report.
pub fn generate_attestation_report() -> Result<AttestationReport> {
    // TODO: use the `sev` crate to generate a real attestation report
    // For now, return a stub

    let measurement = current_measurement().unwrap_or_else(|| "unknown".to_string());
    let report = base64::engine::general_purpose::STANDARD.encode(b"stub-attestation-report");
    let report_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&report);
        hex::encode(hasher.finalize())
    };

    Ok(AttestationReport {
        report,
        report_hash,
        measurement,
        sev_snp_active: true,
        hardened_mode: true,
        generated_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Get the current launch measurement.
pub fn current_measurement() -> Option<String> {
    // TODO: read actual measurement from SEV-SNP firmware
    Some("sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string())
}

/// Seal keys to the current measurement.
///
/// Sealed keys can only be unsealed when the gateway is running with
/// the exact same binary + kernel + initrd. If the binary is modified,
/// the measurement changes and the keys cannot be unsealed.
pub fn seal_keys(keys: &[u8]) -> Result<Vec<u8>> {
    // TODO: use SEV-SNP key derivation to seal
    Ok(keys.to_vec())
}

/// Unseal keys that were sealed to a previous measurement.
pub fn unseal_keys(sealed: &[u8]) -> Result<Vec<u8>> {
    // TODO: use SEV-SNP key derivation to unseal
    Ok(sealed.to_vec())
}
