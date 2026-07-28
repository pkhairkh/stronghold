//! SEV-SNP attestation endpoint.
//!
//! Exposes the gateway's attestation report so the phone can verify
//! the gateway is running on genuine SEV-SNP hardware before approving
//! any session.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use crate::routes::AppState;

#[derive(Debug, Serialize)]
pub struct AttestationResponse {
    /// SEV-SNP attestation report (base64-encoded)
    pub report: String,
    /// SHA-256 hash of the report
    pub report_hash: String,
    /// Launch measurement (hash of binary + kernel + initrd)
    pub measurement: String,
    /// Whether SEV-SNP is active
    pub sev_snp_active: bool,
    /// Whether the gateway booted in hardened mode
    pub hardened_mode: bool,
    /// Timestamp of report generation
    pub generated_at: String,
}

/// Get the current SEV-SNP attestation report.
///
/// The phone fetches this before any WebAuthn ceremony. The WebAuthn
/// challenge includes the report hash, binding the approval to a specific
/// attested state of the gateway.
pub async fn get_report(
    State(_state): State<AppState>,
) -> Result<Json<AttestationResponse>, (StatusCode, String)> {
    let report = crate::tee::generate_attestation_report()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(report))
}
