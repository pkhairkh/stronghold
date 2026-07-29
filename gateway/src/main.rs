//! Stronghold Gateway — Control Plane Binary
//!
//! This is the entry point for the Stronghold control plane. The gateway
//! is a single binary that runs inside an AMD SEV-SNP confidential VM and
//! provides:
//!
//! - HTTP/WebSocket endpoints for agent protocol (ORDER/RESUME/RELEASE/EXTEND)
//! - WebAuthn-based phone approval (multi-credential, quorum)
//! - Post-quantum hybrid cryptography (TLS, audit signatures, push encryption)
//! - k3s scheduler integration for multi-box fleet
//! - Dual-signed audit log (Ed25519 + ML-DSA-65)
//! - SEV-SNP attestation endpoint
//!
//! See `docs/` for the full specification.

// Scaffold-stage allow: will be removed in Wave 11 (Integration & E2E).
#![allow(dead_code)]

use anyhow::Result;
use clap::{Parser, Subcommand};

mod anomaly;
mod audit;
mod crypto;
mod db;
mod images;
mod machines;
mod push;
mod routes;
mod sessions;
mod tee;
mod tenants;
mod watchdog;
mod workflow;

/// Stronghold Gateway command-line interface
#[derive(Parser)]
#[command(name = "stronghold-gateway")]
#[command(version, about = "Stronghold control plane — phone-approved, post-quantum, SEV-SNP attested gateway for AI agents", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to configuration file
    #[arg(short, long, env = "STRONGHOLD_CONFIG")]
    config: Option<String>,

    /// Run in development mode (relaxes SEV-SNP requirement)
    #[arg(long, env = "STRONGHOLD_DEV")]
    dev: bool,

    /// Address to bind on
    #[arg(long, env = "STRONGHOLD_BIND", default_value = "0.0.0.0:8443")]
    bind: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the gateway server (default)
    Serve {
        /// Address to bind on
        #[arg(long, default_value = "0.0.0.0:8443")]
        bind: String,
        /// Run in development mode (skips SEV-SNP check)
        #[arg(long)]
        dev: bool,
    },

    /// Generate SEV-SNP attestation report (for verification)
    Attestation,

    /// Initialize a new Stronghold installation
    Init {
        /// Directory to initialize
        #[arg(long, default_value = "/etc/stronghold")]
        data_dir: String,
    },

    /// Verify the audit log
    AuditVerify {
        /// Tenant ID to verify
        #[arg(long)]
        tenant: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "stronghold_gateway=info,tower_http=info".into()),
        )
        .init();

    // Install aws-lc-rs as the process-level CryptoProvider so that every
    // rustls-backed client (reqwest, kube, oci-distribution) picks the same
    // provider as the TLS server config in crypto::tls. Without this,
    // reqwest::Client::new() panics with "Could not automatically determine
    // the process-level CryptoProvider" the first time it tries to build a
    // rustls client connector.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install aws-lc-rs CryptoProvider");

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Serve { bind, dev }) => {
            tracing::info!("Starting Stronghold Gateway on {}", bind);
            serve(&bind, dev).await
        }
        Some(Commands::Attestation) => {
            tracing::info!("Generating SEV-SNP attestation report");
            print_attestation().await
        }
        Some(Commands::Init { data_dir }) => {
            tracing::info!("Initializing Stronghold in {}", data_dir);
            init(&data_dir).await
        }
        Some(Commands::AuditVerify { tenant }) => {
            tracing::info!("Verifying audit log for tenant {}", tenant);
            audit::verify::verify_tenant(&tenant)
        }
        None => {
            tracing::info!("Starting Stronghold Gateway on {}", cli.bind);
            serve(&cli.bind, cli.dev).await
        }
    }
}

/// Run the gateway server
///
/// In production mode: verifies SEV-SNP, loads TLS cert, serves HTTPS.
/// In dev mode (`dev=true`): skips SEV-SNP check, auto-generates self-signed
/// cert if missing, serves HTTPS with self-signed cert.
async fn serve(bind_addr: &str, dev: bool) -> Result<()> {
    // Verify SEV-SNP is available (unless --dev)
    if dev {
        tracing::warn!("Running in dev mode — SEV-SNP check skipped");
    } else {
        tee::verify_sev_snp_available()?;
    }

    // Initialize database
    let db_pool = db::init_pool("/var/lib/stronghold/stronghold.db")?;
    tracing::info!("Database initialized");

    // Generate/load keys
    let audit_keys =
        crypto::hybrid_sig::AuditKeys::load_or_generate_keys("/var/lib/stronghold/keys/")?;
    let push_keys =
        crypto::hybrid_kem::PushKeys::load_or_generate_keys("/var/lib/stronghold/keys/")?;
    tracing::info!("Cryptographic keys loaded");

    // Ensure TLS certificate exists (auto-generate if missing)
    let keys_dir = "/var/lib/stronghold/keys";
    let cert_path = format!("{}/tls.crt", keys_dir);
    if !std::path::Path::new(&cert_path).exists() {
        tracing::info!("TLS certificate not found — generating self-signed cert");
        crypto::tls::generate_self_signed_cert_files(keys_dir, "localhost")?;
    }

    // Build TLS server config with X25519MLKEM768 hybrid
    let tls_config = crypto::tls::build_server_config_from_files(keys_dir)?;
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(tls_config.into());
    tracing::info!("TLS configured with X25519MLKEM768 hybrid PQ key exchange");

    // Build the axum router
    let (app, app_state) = routes::build_router(db_pool, audit_keys, push_keys);

    // Start the watchdog monitoring loop as a background task
    watchdog::monitor::spawn_watchdog(app_state);

    // Serve with TLS via axum-server
    let addr: std::net::SocketAddr = bind_addr.parse()
        .unwrap_or_else(|_| "0.0.0.0:8443".parse().unwrap());
    tracing::info!("Gateway listening on https://{}", bind_addr);

    axum_server::bind_rustls(addr, rustls_config)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

/// Print SEV-SNP attestation report
async fn print_attestation() -> Result<()> {
    let report = tee::generate_attestation_report()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Initialize a new Stronghold installation
async fn init(data_dir: &str) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    std::fs::create_dir_all(format!("{}/keys", data_dir))?;
    std::fs::create_dir_all(format!("{}/audit", data_dir))?;

    // Initialize database
    let db_path = format!("{}/stronghold.db", data_dir);
    db::init_pool(&db_path)?;

    // Generate keys
    crypto::hybrid_sig::generate_keys(&format!("{}/keys", data_dir))?;
    crypto::hybrid_kem::generate_keys(&format!("{}/keys", data_dir))?;

    // Generate setup password
    let setup_password = generate_setup_password();
    println!("Setup password (save this — it will not be shown again):");
    println!("  {}", setup_password);
    println!();
    println!("Enrollment URL: https://<your-gateway>:8443/setup");

    Ok(())
}

fn generate_setup_password() -> String {
    use rand::Rng;
    let chars: Vec<char> = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
        .chars()
        .collect();
    (0..32)
        .map(|_| chars[rand::thread_rng().gen_range(0..chars.len())])
        .collect()
}
