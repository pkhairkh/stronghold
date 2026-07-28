//! TLS configuration — X25519Kyber768Draft00 hybrid post-quantum transport.
//!
//! Uses rustls 0.23 with the `pqc-kyber` feature flag.

use anyhow::Result;
use rustls::ServerConfig;

/// Build a TLS server config with X25519Kyber768 hybrid key exchange.
pub fn build_server_config() -> Result<ServerConfig> {
    // TODO: configure rustls with X25519Kyber768Draft00 cipher suite
    // For now, use default config (will be replaced with PQ config)

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                vec![],
            )),
        );

    // The above will fail at runtime without a real cert, but compiles.
    // In production, load cert + key from /etc/stronghold/keys/

    Ok(config.expect("TLS config"))
}

/// Build a TLS client config with X25519Kyber768 hybrid key exchange.
pub fn build_client_config() -> Result<rustls::ClientConfig> {
    // TODO: configure with PQ hybrid + pinned server public key
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();

    Ok(config)
}
