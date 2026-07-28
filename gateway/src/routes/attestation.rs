//! SEV-SNP attestation endpoint.
//!
//! Exposes the gateway's attestation report so the phone can verify
//! the gateway is running on genuine SEV-SNP hardware before approving
//! any session.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use crate::routes::AppState;

/// Get the current SEV-SNP attestation report.
///
/// The phone fetches this before any WebAuthn ceremony. The WebAuthn
/// challenge includes the report hash, binding the approval to a specific
/// attested state of the gateway.
pub async fn get_report(
    State(_state): State<AppState>,
) -> Result<Json<crate::tee::AttestationReport>, (StatusCode, String)> {
    let report = crate::tee::generate_attestation_report()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(report))
}
