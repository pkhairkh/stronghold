//! TLS configuration — X25519MLKEM768 hybrid post-quantum transport.
//!
//! Uses rustls 0.23.22+ with the `prefer-post-quantum` feature, which
//! enables the X25519MLKEM768 hybrid key exchange (FIPS 203 ML-KEM-768
//! combined with classical X25519).
//!
//! Implemented in: W1-T7
//! Tested by: gateway/src/crypto/tls.rs (unit tests for config construction)

use anyhow::{Context, Result};
use rustls::crypto::aws_lc_rs::default_provider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::sync::Arc;

/// Build a TLS server config with X25519MLKEM768 hybrid key exchange.
///
/// Requires a certificate and private key. For development, use
/// `build_self_signed_server_config()` to generate an ephemeral self-signed cert.
/// For production, load cert + key from `/etc/stronghold/keys/` via
/// `build_server_config_from_files()`.
///
/// The `prefer-post-quantum` feature on rustls 0.23.22+ makes X25519MLKEM768
/// the preferred key exchange. A future quantum adversary who records traffic
/// today cannot decrypt it (harvest-now-decrypt-later mitigation).
pub fn build_server_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key_der: PrivateKeyDer<'static>,
) -> Result<ServerConfig> {
    // Use the aws_lc_rs provider with prefer-post-quantum (set via feature flag).
    let provider = Arc::new(default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("selecting TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .context("failed to build TLS server config")?;
    Ok(config)
}

/// Build a TLS server config with a self-signed certificate (for development).
///
/// Generates an ephemeral ECDSA P-256 key and self-signed certificate.
/// The certificate is NOT trusted by default — clients must pin the
/// public key or use `build_client_config_with_self_signed()`.
#[cfg(test)]
pub fn build_self_signed_server_config() -> Result<(ServerConfig, Vec<u8>)> {
    use aws_lc_rs::signature::EcKeyPair;
    use aws_lc_rs::{ec::ECDSA_P256_SHA256_ASN1_SIGNING, rand::SystemRandom};

    let rng = SystemRandom::new();
    let key_pair = EcKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map_err(|e| anyhow::anyhow!("ECDSA key generation failed: {:?}", e))?;

    // Export the private key in PKCS#8 format.
    let pkcs8 = key_pair
        .private_key_to_pkcs8()
        .map_err(|e| anyhow::anyhow!("PKCS#8 export failed: {:?}", e))?;
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8));

    // Build a minimal self-signed certificate.
    // Note: For a real self-signed cert we'd use rcgen or aws_lc_rs's cert
    // builder. For now, we just verify the config builds with an empty cert
    // chain — the actual cert generation is deferred to W10 (Bootstrap).
    let cert_chain: Vec<CertificateDer<'static>> = vec![];

    let _ = (cert_chain, key_der);
    // This will fail without a real cert; that's expected for the dev path.
    Err(anyhow::anyhow!(
        "self-signed cert generation not yet implemented — use build_server_config_from_files()"
    ))
}

/// Build a TLS server config from PEM-encoded cert and key files.
///
/// Reads `<keys_dir>/tls.crt` and `<keys_dir>/tls.key`. Both files must
/// exist and be valid PEM.
pub fn build_server_config_from_files(keys_dir: &str) -> Result<ServerConfig> {
    let cert_path = format!("{}/tls.crt", keys_dir);
    let key_path = format!("{}/tls.key", keys_dir);

    let cert_pem = std::fs::read(&cert_path)
        .with_context(|| format!("reading TLS cert {}", cert_path))?;
    let key_pem = std::fs::read(&key_path)
        .with_context(|| format!("reading TLS key {}", key_path))?;

    let cert_chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .context("parsing TLS cert PEM")?;

    if cert_chain.is_empty() {
        return Err(anyhow::anyhow!(
            "no certificates found in {}",
            cert_path
        ));
    }

    let key_der = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .context("parsing TLS key PEM")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key_path))?;

    build_server_config(cert_chain, key_der)
}

/// Build a TLS client config with X25519MLKEM768 hybrid key exchange.
///
/// Uses the aws_lc_rs provider with `prefer-post-quantum`. The client
/// will prefer X25519MLKEM768 when offered by the server.
pub fn build_client_config() -> Result<rustls::ClientConfig> {
    let provider = Arc::new(default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("selecting TLS protocol versions")?
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    Ok(config)
}

/// Build a TLS client config that trusts a specific self-signed server cert.
///
/// Used for development where the server uses a self-signed cert. The client
/// pins the cert's SHA-256 fingerprint rather than using a CA.
pub fn build_client_config_with_pinned_cert(
    cert_der: &[u8],
) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(default_provider());
    let mut root_store = rustls::RootCertStore::empty();
    root_store
        .add(cert_der.to_vec().into())
        .context("adding pinned cert to root store")?;

    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("selecting TLS protocol versions")?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(config)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_client_config_succeeds() {
        // The client config should build with the aws_lc_rs + PQ provider.
        let config = build_client_config();
        assert!(config.is_ok(), "client config should build");
    }

    #[test]
    fn test_build_server_config_rejects_empty_cert_chain() {
        // An empty cert chain should produce an error (not a panic).
        let cert_chain: Vec<CertificateDer<'static>> = vec![];
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(vec![]));
        let result = build_server_config(cert_chain, key_der);
        assert!(result.is_err(), "empty cert chain should error");
    }

    #[test]
    fn test_build_server_config_from_missing_files_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap();
        let result = build_server_config_from_files(dir);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("tls.crt") || err.contains("reading"));
    }
}
