//! No-SEV-SNP stub — used when compiled with `--features no-sev-snp`.
//!
//! All functions return stubs. The gateway runs without TEE protection.
//! Audit log entries will lack `sev_snp_report` and `audit verify` will
//! warn (but not fail).
//!
//! `seal_keys()` / `unseal_keys()` are pass-throughs (no encryption) so
//! that the dev box can exercise the rest of the gateway (audit log
//! signing, push key generation, etc.) without needing SEV-SNP hardware.
//! The shared [`crate::tee::sealing`] module contains the real
//! HKDF + AES-256-GCM key-sealing primitives used by the `sev-snp`
//! feature path; tests for those primitives live there and run under
//! both feature flags.

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
///
/// `sev_snp_active` is always `false`. `report` / `report_hash` /
/// `measurement` are documented placeholder strings so consumers can
/// detect the stub by string comparison rather than just the boolean
/// (defense in depth).
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
///
/// In the dev fallback there is no measurement to bind to, so the keys
/// are returned unencrypted. Production deployments must use
/// `--features sev-snp` (the default) which provides real
/// measurement-bound sealing via [`crate::tee::sev_snp::seal_keys`].
pub fn seal_keys(keys: &[u8]) -> Result<Vec<u8>> {
    Ok(keys.to_vec())
}

/// No-op unseal (returns input unchanged).
pub fn unseal_keys(sealed: &[u8]) -> Result<Vec<u8>> {
    Ok(sealed.to_vec())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- W7-T7: stub correctness ---

    #[test]
    fn test_stub_reports_sev_snp_inactive() {
        let report = generate_attestation_report().expect("stub must always succeed");
        assert!(!report.sev_snp_active, "no-sev-snp stub must report sev_snp_active=false");
        assert!(!report.hardened_mode, "no-sev-snp stub must report hardened_mode=false");
    }

    #[test]
    fn test_stub_verify_sev_snp_available_is_ok() {
        // In no-sev-snp mode, the check always succeeds (we're not
        // promising SEV-SNP, so absence of /dev/sev-guest is fine).
        assert!(verify_sev_snp_available().is_ok());
    }

    #[test]
    fn test_stub_current_measurement_is_none() {
        assert!(current_measurement().is_none());
    }

    #[test]
    fn test_stub_report_has_documented_placeholder_strings() {
        let report = generate_attestation_report().expect("stub must always succeed");
        assert_eq!(report.report, "no-sev-snp");
        assert_eq!(report.report_hash, "n/a");
        assert_eq!(report.measurement, "n/a");
    }

    #[test]
    fn test_stub_generated_at_is_valid_rfc3339() {
        let report = generate_attestation_report().expect("stub must always succeed");
        assert!(
            chrono::DateTime::parse_from_rfc3339(&report.generated_at).is_ok(),
            "generated_at must be RFC 3339, got: {}",
            report.generated_at
        );
    }

    #[test]
    fn test_stub_seal_unseal_is_pass_through() {
        // In no-sev-snp mode, seal/unseal are pass-throughs so the rest of
        // the gateway (audit log, push keys) works on the dev box.
        let plaintext = b"any-key-material";
        let sealed = seal_keys(plaintext).expect("seal must succeed");
        assert_eq!(&sealed[..], &plaintext[..], "stub seal must be pass-through");
        let unsealed = unseal_keys(&sealed).expect("unseal must succeed");
        assert_eq!(&unsealed[..], &plaintext[..], "stub unseal must be pass-through");
    }

    #[test]
    fn test_stub_report_serializes_to_json() {
        let report = generate_attestation_report().expect("stub must always succeed");
        let json = serde_json::to_value(&report).expect("JSON serialize");
        let obj = json.as_object().expect("JSON object");
        for key in [
            "report",
            "report_hash",
            "measurement",
            "sev_snp_active",
            "hardened_mode",
            "generated_at",
        ] {
            assert!(obj.contains_key(key), "JSON must contain key: {}", key);
        }
        assert_eq!(obj["sev_snp_active"], serde_json::Value::Bool(false));
        assert_eq!(obj["hardened_mode"], serde_json::Value::Bool(false));
    }
}
