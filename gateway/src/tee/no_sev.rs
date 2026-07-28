//! No-SEV-SNP stub — used when compiled with `--features no-sev-snp`.
//!
//! All functions return stubs. The gateway runs without TEE protection.
//! Audit log entries will lack `sev_snp_report` and `audit verify` will
//! warn (but not fail).

use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AttestationReport {
    pub report: String,
    pub report_hash: String,
    pub measurement: String,
    pub sev_snp_active: bool,
    pub hardened_mode: bool,
    pub generated_at: String,
}

/// Always returns Ok (no SEV-SNP required).
pub fn verify_sev_snp_available() -> Result<()> {
    tracing::warn!("Running without SEV-SNP (compiled with --features no-sev-snp)");
    Ok(())
}

/// Returns a stub attestation report.
pub fn generate_attestation_report() -> Result<AttestationReport> {
    Ok(AttestationReport {
        report: "no-sev-snp".to_string(),
        report_hash: "n/a".to_string(),
        measurement: "n/a".to_string(),
        sev_snp_active: false,
        hardened_mode: false,
        generated_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Returns None (no measurement available).
pub fn current_measurement() -> Option<String> {
    None
}

/// No-op seal (returns input unchanged).
pub fn seal_keys(keys: &[u8]) -> Result<Vec<u8>> {
    Ok(keys.to_vec())
}

/// No-op unseal (returns input unchanged).
pub fn unseal_keys(sealed: &[u8]) -> Result<Vec<u8>> {
    Ok(sealed.to_vec())
}
