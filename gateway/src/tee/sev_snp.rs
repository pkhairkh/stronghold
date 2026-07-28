//! SEV-SNP attestation driver.
//!
//! Compiled when the `sev-snp` cargo feature is enabled (the default).
//! Provides:
//! - [`verify_sev_snp_available()`] — checks `/dev/sev-guest` exists
//! - [`generate_attestation_report()`] — produces a signed attestation report
//! - [`current_measurement()`] — returns the launch measurement
//! - [`seal_keys()`] / [`unseal_keys()`] — seal keys to the launch measurement
//!
//! ## What's real vs. stubbed (Wave 7 / v1.0)
//!
//! | Function | Real on SEV-SNP hardware | Dev-box fallback |
//! |---|---|---|
//! | `verify_sev_snp_available()` | Yes — checks `/dev/sev-guest` | Returns Err (no /dev/sev-guest) |
//! | `generate_attestation_report()` | Yes — `sev::firmware::guest::Firmware::get_report()` | Returns stub with `sev_snp_active: false` |
//! | `current_measurement()` | Yes — first 48 bytes of the attestation report | Returns `None` |
//! | `seal_keys()` / `unseal_keys()` | Yes — hardware-derived key via `get_derived_key()` + AES-256-GCM | HKDF from measurement string + AES-256-GCM (via [`crate::tee::sealing`]) |
//!
//! ## Path to full SEV-SNP on real hardware
//!
//! 1. Provision a Vultr HF plan with SEV-SNP enabled (W7-T1).
//! 2. Boot the gateway inside the SEV-SNP guest.
//! 3. `/dev/sev-guest` is present; `Firmware::open()` succeeds.
//! 4. `generate_attestation_report()` calls `fw.get_report(None, None, Some(1))`.
//! 5. `current_measurement()` returns the hex-encoded 48-byte measurement.
//! 6. `seal_keys()` calls `fw.get_derived_key(None, DerivedKey { ... })` to
//!    fetch a hardware-derived 32-byte key mixed with the launch measurement,
//!    then AES-256-GCM encrypts the keys with that key.
//!
//! See `docs/SEV_SNP.md` for the full production attestation flow.

use anyhow::Result;
use base64::Engine;
use serde::Serialize;

use crate::tee::sealing;

/// Attestation report returned by the gateway's `/attestation` endpoint.
///
/// On real SEV-SNP hardware, `report` is the bincode-serialized
/// `sev::firmware::guest::AttestationReport` (a `#[repr(C)]` struct
/// signed by the AMD VCEK), base64-encoded. The phone verifies the
/// signature against AMD's published VCEK certificate chain.
///
/// On the dev-box fallback, `report` is the literal string
/// `"stub-attestation-report"` and `sev_snp_active` is `false`.
#[derive(Debug, Serialize)]
pub struct AttestationReport {
    /// Base64-encoded attestation report (bincode-serialized on real HW).
    pub report: String,
    /// SHA-256 hex of `report`. Bound into the WebAuthn challenge so the
    /// phone signs over the current attested state of the gateway.
    pub report_hash: String,
    /// Hex-encoded launch measurement (`sha256:<64 hex chars>` on dev,
    /// `sha384:<96 hex chars>` on real SEV-SNP — the 48-byte measurement
    /// field of the firmware report).
    pub measurement: String,
    /// `true` only when running inside a real SEV-SNP guest.
    pub sev_snp_active: bool,
    /// `true` when both SEV-SNP is active *and* keys are sealed to the
    /// measurement. `false` on dev box.
    pub hardened_mode: bool,
    /// RFC 3339 timestamp of report generation.
    pub generated_at: String,
}

/// Verify that SEV-SNP is available on this machine.
///
/// Returns `Ok(())` only if `/dev/sev-guest` is present (the guest-side
/// device node — distinct from the host-side `/dev/sev`).
pub fn verify_sev_snp_available() -> Result<()> {
    let dev_sev_guest = std::path::Path::new("/dev/sev-guest");
    let dev_sev = std::path::Path::new("/dev/sev");
    if !dev_sev_guest.exists() {
        return Err(anyhow::anyhow!(
            "SEV-SNP not available (/dev/sev-guest not found). \
             The guest-side device node is created by the kernel's \
             `sev-guest` driver when the VM is launched with SEV-SNP \
             enabled. \
             Either boot inside an SEV-SNP guest, or build with \
             --features no-sev-snp for development."
        ));
    }
    tracing::info!(
        "/dev/sev-guest detected (also /dev/sev present: {})",
        dev_sev.exists()
    );
    Ok(())
}

/// Try to open the SEV-SNP firmware handle. Returns `None` if `/dev/sev-guest`
/// is not present (e.g., dev box, or a non-SEV Vultr plan).
///
/// On success, returns a `sev::firmware::guest::Firmware` that can be used
/// to call `get_report()` and `get_derived_key()`.
fn try_open_firmware() -> Option<sev::firmware::guest::Firmware> {
    match sev::firmware::guest::Firmware::open() {
        Ok(fw) => Some(fw),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not open /dev/sev-guest; falling back to non-TEE attestation stub"
            );
            None
        }
    }
}

/// Generate an attestation report.
///
/// On real SEV-SNP hardware:
/// 1. Open `/dev/sev-guest`.
/// 2. Call `fw.get_report(None, None, Some(1))` to fetch a signed
///    `sev::firmware::guest::AttestationReport` from the AMD Secure
///    Processor. The report's `measurement` field (48 bytes) is the launch
///    measurement of the guest.
/// 3. Serialize the report with `bincode`, base64-encode it, hash it with
///    SHA-256, and return.
///
/// On the dev-box fallback (no `/dev/sev-guest`):
/// - `report` is `"stub-attestation-report"` (base64 of that literal).
/// - `sev_snp_active` is `false`.
/// - `measurement` is the placeholder string from [`current_measurement`].
pub fn generate_attestation_report() -> Result<AttestationReport> {
    if let Some(mut fw) = try_open_firmware() {
        // Real SEV-SNP path. VMPL=1 is the conventional value for the guest
        // (VMPL 0 is reserved for the host; the guest runs at VMPL >= 1).
        let report_result = fw.get_report(None, None, Some(1));
        match report_result {
            Ok(snp_report) => {
                // Serialize the report with bincode so the phone can
                // deserialize the exact `#[repr(C)]` struct and verify
                // the VCEK signature.
                let report_bytes = bincode::serialize(&snp_report)
                    .map_err(|e| anyhow::anyhow!("bincode serialize attestation report: {}", e))?;
                let report_b64 = base64::engine::general_purpose::STANDARD.encode(&report_bytes);

                let report_hash = sha256_hex(&report_b64);

                // 48-byte measurement field → hex string.
                let measurement = format!(
                    "sha384:{}",
                    hex::encode(snp_report.measurement)
                );

                tracing::info!(
                    measurement = %measurement,
                    report_hash = %report_hash,
                    "generated real SEV-SNP attestation report"
                );

                return Ok(AttestationReport {
                    report: report_b64,
                    report_hash,
                    measurement,
                    sev_snp_active: true,
                    hardened_mode: true,
                    generated_at: chrono::Utc::now().to_rfc3339(),
                });
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "SEV-SNP firmware get_report failed; falling back to stub"
                );
            }
        }
    }

    // Dev-box fallback.
    let measurement = current_measurement().unwrap_or_else(|| "n/a".to_string());
    let report = base64::engine::general_purpose::STANDARD.encode(b"stub-attestation-report");
    let report_hash = sha256_hex(&report);
    tracing::warn!(
        measurement = %measurement,
        "returning stub attestation report (no /dev/sev-guest)"
    );
    Ok(AttestationReport {
        report,
        report_hash,
        measurement,
        sev_snp_active: false,
        hardened_mode: false,
        generated_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Get the current launch measurement.
///
/// On real SEV-SNP hardware, this opens `/dev/sev-guest` and reads the
/// `measurement` field from a fresh attestation report. The measurement is
/// a 48-byte SHA-384 digest computed by the AMD Secure Processor at launch
/// over the guest's initial memory contents (kernel, initrd, firmware
/// state). It is signed by the VCEK, so it cannot be spoofed.
///
/// Returns `None` on the dev box (no /dev/sev-guest).
pub fn current_measurement() -> Option<String> {
    let mut fw = try_open_firmware()?;
    match fw.get_report(None, None, Some(1)) {
        Ok(snp_report) => Some(format!(
            "sha384:{}",
            hex::encode(snp_report.measurement)
        )),
        Err(e) => {
            tracing::warn!(error = %e, "could not read measurement from /dev/sev-guest");
            None
        }
    }
}

/// Seal keys to the current launch measurement.
///
/// Sealed keys can only be unsealed when the gateway is running with
/// the exact same binary + kernel + initrd. If the binary is modified,
/// the measurement changes and the keys cannot be unsealed.
///
/// ## Implementation
///
/// On real SEV-SNP hardware:
/// 1. Open `/dev/sev-guest`.
/// 2. Call `fw.get_derived_key(None, DerivedKey::new(false,
///    GuestFieldSelect(0b1000), 0, 0, 0))` — the `GuestFieldSelect` bit 3
///    mixes the launch measurement into the derived key.
/// 3. AES-256-GCM encrypt `keys` with the derived 32-byte key.
///
/// On the dev-box fallback, derive the AES key via HKDF-SHA256 from the
/// placeholder measurement string. This exercises the same AES-GCM code
/// path and is testable without hardware.
pub fn seal_keys(keys: &[u8]) -> Result<Vec<u8>> {
    if let Some(mut fw) = try_open_firmware() {
        // Real SEV-SNP: derive a hardware key mixed with the launch
        // measurement (GuestFieldSelect bit 3 = measurement).
        let gfs = sev::firmware::guest::GuestFieldSelect(1 << 3);
        let dk = sev::firmware::guest::DerivedKey::new(false, gfs, 0, 0, 0);
        match fw.get_derived_key(None, dk) {
            Ok(hw_key) => {
                return sealing::seal_with_key(&hw_key, keys);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "get_derived_key failed; falling back to HKDF-from-measurement sealing"
                );
            }
        }
    }

    // Dev fallback: HKDF from the current measurement string. If the
    // measurement is unavailable, fall back to a fixed dev-only string so
    // the call doesn't hard-fail on the dev box (callers like the audit
    // log writer call seal_keys unconditionally).
    let measurement = current_measurement()
        .unwrap_or_else(|| "sha256:dev-fallback-no-measurement".to_string());
    sealing::seal_with_measurement(&measurement, keys)
}

/// Unseal keys that were sealed to a previous measurement.
///
/// Symmetric counterpart of [`seal_keys`]: derives the same key (from the
/// hardware-derived key on real SEV-SNP, or from the measurement string
/// on the dev fallback) and AES-GCM decrypts.
///
/// Returns an error if the measurement has changed (binary modified,
/// kernel upgraded, etc.) — the GCM authentication tag will not verify.
pub fn unseal_keys(sealed: &[u8]) -> Result<Vec<u8>> {
    if let Some(mut fw) = try_open_firmware() {
        let gfs = sev::firmware::guest::GuestFieldSelect(1 << 3);
        let dk = sev::firmware::guest::DerivedKey::new(false, gfs, 0, 0, 0);
        match fw.get_derived_key(None, dk) {
            Ok(hw_key) => {
                return sealing::unseal_with_key(&hw_key, sealed);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "get_derived_key failed; falling back to HKDF-from-measurement unsealing"
                );
            }
        }
    }

    let measurement = current_measurement()
        .unwrap_or_else(|| "sha256:dev-fallback-no-measurement".to_string());
    sealing::unseal_with_measurement(&measurement, sealed)
}

/// SHA-256 hex digest of a string.
fn sha256_hex(s: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- W7-T5: attestation report structure ---

    #[test]
    fn test_attestation_report_has_all_fields() {
        let report = generate_attestation_report().expect("attestation report must generate");

        // Every field must be a non-empty string (or a bool).
        assert!(!report.report.is_empty(), "report field must not be empty");
        assert!(!report.report_hash.is_empty(), "report_hash field must not be empty");
        assert!(!report.measurement.is_empty(), "measurement field must not be empty");
        assert!(!report.generated_at.is_empty(), "generated_at field must not be empty");

        // report_hash must be a 64-char lowercase hex SHA-256 digest.
        assert_eq!(report.report_hash.len(), 64, "report_hash must be SHA-256 hex");
        assert!(
            report.report_hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "report_hash must be lowercase hex"
        );

        // generated_at must parse as RFC 3339.
        assert!(
            chrono::DateTime::parse_from_rfc3339(&report.generated_at).is_ok(),
            "generated_at must be a valid RFC 3339 timestamp, got: {}",
            report.generated_at
        );
    }

    #[test]
    fn test_attestation_report_field_types() {
        let report = generate_attestation_report().expect("attestation report must generate");

        // Verify the field types via static assertions (the compiler already
        // enforces these, but the test makes the contract explicit).
        let _: &String = &report.report;
        let _: &String = &report.report_hash;
        let _: &String = &report.measurement;
        let _: &bool = &report.sev_snp_active;
        let _: &bool = &report.hardened_mode;
        let _: &String = &report.generated_at;
    }

    #[test]
    fn test_attestation_report_serializes_to_json() {
        // The /attestation endpoint returns this struct as JSON; verify it
        // serializes cleanly with the expected keys.
        let report = generate_attestation_report().expect("attestation report must generate");
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
        assert!(obj["sev_snp_active"].is_boolean());
        assert!(obj["hardened_mode"].is_boolean());
    }

    #[test]
    fn test_attestation_report_measurement_format() {
        // On the dev box (no /dev/sev-guest) the report falls back to the
        // placeholder measurement string. On real SEV-SNP hardware it's
        // `sha384:<96 hex chars>`.
        let report = generate_attestation_report().expect("attestation report must generate");
        assert!(
            report.measurement.starts_with("sha256:")
                || report.measurement.starts_with("sha384:")
                || report.measurement == "n/a",
            "measurement must be a sha256:/sha384: prefixed hex string or 'n/a', got: {}",
            report.measurement
        );
    }

    #[test]
    fn test_attestation_report_hash_matches_report() {
        // report_hash must equal SHA-256 hex of the report field.
        let report = generate_attestation_report().expect("attestation report must generate");
        let expected = sha256_hex(&report.report);
        assert_eq!(report.report_hash, expected);
    }

    // --- W7-T7: stub correctness (when /dev/sev-guest is absent) ---

    #[test]
    fn test_dev_box_fallback_returns_sev_snp_inactive() {
        // On the dev box (no /dev/sev-guest) the report must mark
        // sev_snp_active=false. On real SEV-SNP hardware this test would
        // see true (and that's also correct).
        let report = generate_attestation_report().expect("attestation report must generate");
        if !std::path::Path::new("/dev/sev-guest").exists() {
            assert!(!report.sev_snp_active, "dev box must report sev_snp_active=false");
            assert!(!report.hardened_mode, "dev box must report hardened_mode=false");
        }
    }

    #[test]
    fn test_verify_sev_snp_available_returns_err_on_dev_box() {
        if !std::path::Path::new("/dev/sev-guest").exists() {
            let result = verify_sev_snp_available();
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("/dev/sev-guest"),
                "error must mention /dev/sev-guest, got: {}",
                err
            );
        }
    }

    // --- W7-T3: key sealing round-trip (dev fallback path) ---

    #[test]
    fn test_seal_unseal_keys_round_trip() {
        // Uses the dev fallback path (no /dev/sev-guest) — exercises the
        // HKDF-from-measurement + AES-256-GCM code path via the shared
        // `sealing` module.
        let plaintext = b"audit-signing-key-material-32-bytes!";
        let sealed = seal_keys(plaintext).expect("seal_keys must succeed");
        assert_ne!(&sealed[..], &plaintext[..], "sealing must produce ciphertext");
        let unsealed = unseal_keys(&sealed).expect("unseal_keys must succeed");
        assert_eq!(&unsealed[..], &plaintext[..], "round-trip must recover plaintext");
    }

    #[test]
    fn test_seal_keys_produces_nonce_prefixed_ciphertext() {
        let plaintext = b"some-key-material";
        let sealed = seal_keys(plaintext).expect("seal_keys must succeed");
        // 12-byte nonce + ciphertext (== plaintext len) + 16-byte GCM tag.
        assert_eq!(sealed.len(), 12 + plaintext.len() + 16);
    }

    #[test]
    fn test_seal_keys_is_non_deterministic() {
        // Random nonce per call → sealing the same plaintext twice yields
        // different ciphertexts (semantic security).
        let plaintext = b"identical";
        let s1 = seal_keys(plaintext).expect("seal_keys must succeed");
        let s2 = seal_keys(plaintext).expect("seal_keys must succeed");
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_unseal_keys_rejects_short_input() {
        let result = unseal_keys(b"too-short");
        assert!(result.is_err());
    }
}
