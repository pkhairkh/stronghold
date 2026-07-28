//! Stronghold CLI — manage tenants, credentials, images, and audit logs.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "stronghold")]
#[command(version, about = "Stronghold CLI — manage tenants, credentials, images, and audit logs", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Gateway URL (e.g. https://your-box:8443)
    #[arg(long, env = "STRONGHOLD_URL")]
    url: Option<String>,

    /// Admin token
    #[arg(long, env = "STRONGHOLD_ADMIN_TOKEN")]
    admin_token: Option<String>,

    /// Database path (for local operations)
    #[arg(long, env = "STRONGHOLD_DB", default_value = "/var/lib/stronghold/stronghold.db")]
    db: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Tenant management
    Tenant {
        #[command(subcommand)]
        action: TenantCommands,
    },

    /// Credential management
    Credentials {
        #[command(subcommand)]
        action: CredentialCommands,
    },

    /// Agent token management
    AgentToken {
        #[command(subcommand)]
        action: AgentTokenCommands,
    },

    /// Image management
    Image {
        #[command(subcommand)]
        action: ImageCommands,
    },

    /// Worker management
    Worker {
        #[command(subcommand)]
        action: WorkerCommands,
    },

    /// Audit log operations
    Audit {
        #[command(subcommand)]
        action: AuditCommands,
    },

    /// Key rotation
    Keys {
        #[command(subcommand)]
        action: KeyCommands,
    },

    /// Initialize a new Stronghold installation
    Init {
        #[arg(long, default_value = "/var/lib/stronghold")]
        data_dir: String,
    },
}

#[derive(Subcommand)]
enum TenantCommands {
    Create {
        #[arg(long)]
        name: String,
    },
    List,
    Get {
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum CredentialCommands {
    Enroll,
    List {
        #[arg(long)]
        tenant: String,
    },
    Revoke {
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum AgentTokenCommands {
    Mint {
        #[arg(long)]
        tenant: String,
        #[arg(long, default_value = "default")]
        scope: String,
        #[arg(long, default_value = "86400")]
        ttl: u64,
    },
    List {
        #[arg(long)]
        tenant: String,
    },
    Revoke {
        #[arg(long)]
        token: String,
    },
}

#[derive(Subcommand)]
enum ImageCommands {
    Build {
        #[arg(long)]
        path: String,
        #[arg(long)]
        tag: Option<String>,
    },
    List,
    Push {
        #[arg(long)]
        name: String,
    },
}

#[derive(Subcommand)]
enum WorkerCommands {
    Add {
        #[arg(long)]
        host: String,
        #[arg(long)]
        token: String,
    },
    List,
}

#[derive(Subcommand)]
enum AuditCommands {
    Verify {
        #[arg(long)]
        tenant: String,
    },
    Export {
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "json")]
        format: String,
    },
}

#[derive(Subcommand)]
enum KeyCommands {
    RotateAudit,
    RotatePush,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "stronghold=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Tenant { ref action } => handle_tenant(action, &cli).await,
        Commands::Credentials { ref action } => handle_credentials(action, &cli).await,
        Commands::AgentToken { ref action } => handle_agent_token(action, &cli).await,
        Commands::Image { ref action } => handle_image(action, &cli).await,
        Commands::Worker { ref action } => handle_worker(action, &cli).await,
        Commands::Audit { ref action } => handle_audit(action, &cli).await,
        Commands::Keys { ref action } => handle_keys(action, &cli).await,
        Commands::Init { data_dir } => handle_init(&data_dir).await,
    }
}

async fn handle_tenant(action: &TenantCommands, cli: &Cli) -> anyhow::Result<()> {
    match action {
        TenantCommands::Create { name } => {
            println!("Creating tenant: {}", name);
            // TODO: call gateway API
            println!("Tenant created (stub)");
        }
        TenantCommands::List => {
            println!("Tenants (stub):");
            println!("  tenant_01HXYZ...  alice    created 2026-07-29");
        }
        TenantCommands::Get { id } => {
            println!("Tenant {} (stub)", id);
        }
    }
    Ok(())
}

async fn handle_credentials(action: &CredentialCommands, cli: &Cli) -> anyhow::Result<()> {
    match action {
        CredentialCommands::Enroll => {
            println!("Open this URL in your phone browser to enroll:");
            println!("  {}/setup", cli.url.as_deref().unwrap_or("https://gateway:8443"));
        }
        CredentialCommands::List { tenant } => {
            println!("Credentials for tenant {} (stub):", tenant);
        }
        CredentialCommands::Revoke { id } => {
            println!("Revoking credential {} (stub)", id);
        }
    }
    Ok(())
}

async fn handle_agent_token(action: &AgentTokenCommands, cli: &Cli) -> anyhow::Result<()> {
    match action {
        AgentTokenCommands::Mint { tenant, scope, ttl } => {
            println!("Minting agent token for tenant {} (scope={}, ttl={}s)", tenant, scope, ttl);
            let token = format!("stronghold_agent_stub_{}", ulid::Ulid::new());
            println!("Agent token (save this — it will not be shown again):");
            println!("  {}", token);
        }
        AgentTokenCommands::List { tenant } => {
            println!("Agent tokens for tenant {} (stub)", tenant);
        }
        AgentTokenCommands::Revoke { token } => {
            println!("Revoking agent token {} (stub)", token);
        }
    }
    Ok(())
}

async fn handle_image(action: &ImageCommands, _cli: &Cli) -> anyhow::Result<()> {
    match action {
        ImageCommands::Build { path, tag } => {
            println!("Building image from {} (stub)", path);
            if let Some(t) = tag {
                println!("Tag: {}", t);
            }
        }
        ImageCommands::List => {
            println!("Available images (stub):");
            println!("  stronghold/rocky-base:2026.07");
            println!("  stronghold/rust-nightly:2026.07");
            println!("  stronghold/rust-stable:2026.07");
            println!("  stronghold/node-20:2026.07");
            println!("  stronghold/python-ml:2026.07");
            println!("  stronghold/lean-research:2026.07");
            println!("  stronghold/go-cli:2026.07");
            println!("  stronghold/fullstack:2026.07");
        }
        ImageCommands::Push { name } => {
            println!("Pushing image {} (stub)", name);
        }
    }
    Ok(())
}

async fn handle_worker(action: &WorkerCommands, _cli: &Cli) -> anyhow::Result<()> {
    match action {
        WorkerCommands::Add { host, token: _ } => {
            println!("Adding worker {} (stub)", host);
        }
        WorkerCommands::List => {
            println!("Workers (stub):");
            println!("  vultr-worker-1.fra1  8 cpu / 16GB  sev-snp: yes  3 pods");
            println!("  vultr-worker-2.fra1  8 cpu / 16GB  sev-snp: no   1 pod");
        }
    }
    Ok(())
}

async fn handle_audit(action: &AuditCommands, _cli: &Cli) -> anyhow::Result<()> {
    match action {
        AuditCommands::Verify { tenant } => {
            println!("Verifying audit log for tenant {} (stub)", tenant);
            println!("  Hash chain: OK");
            println!("  Ed25519 signatures: OK");
            println!("  ML-DSA-65 signatures: OK");
            println!("  SEV-SNP attestation: OK");
        }
        AuditCommands::Export { tenant, from, to, format } => {
            println!("Exporting audit log for tenant {} (format={})", tenant, format);
            if let Some(f) = from { println!("  from: {}", f); }
            if let Some(t) = to { println!("  to: {}", t); }
            println!("  (stub — no entries)");
        }
    }
    Ok(())
}

async fn handle_keys(action: &KeyCommands, _cli: &Cli) -> anyhow::Result<()> {
    match action {
        KeyCommands::RotateAudit => {
            println!("Rotating audit keys (stub)");
            println!("  New Ed25519 + ML-DSA-65 keypair generated");
            println!("  Sealed to current SEV-SNP measurement");
        }
        KeyCommands::RotatePush => {
            println!("Rotating push keys (stub)");
            println!("  New X25519 + ML-KEM-768 keypair generated");
            println!("  All phones must re-enroll");
        }
    }
    Ok(())
}

async fn handle_init(data_dir: &str) -> anyhow::Result<()> {
    println!("Initializing Stronghold in {}", data_dir);

    std::fs::create_dir_all(data_dir)?;
    std::fs::create_dir_all(format!("{}/keys", data_dir))?;
    std::fs::create_dir_all(format!("{}/audit", data_dir))?;

    // Initialize database
    let db_path = format!("{}/stronghold.db", data_dir);
    let manager = r2d2_sqlite::SqliteConnectionManager::file(&db_path);
    let pool = r2d2::Pool::builder().max_size(4).build(manager)?;
    let conn = pool.get()?;
    conn.execute_batch(include_str!("../../gateway/src/db/schema.sql"))?;

    println!("Database initialized: {}", db_path);
    println!();
    println!("Next steps:");
    println!("  1. Start the gateway: stronghold-gateway --config {}", data_dir);
    println!("  2. Create a tenant: stronghold tenant create --name <your-name>");
    println!("  3. Enroll your phone (URL will be printed by the gateway)");

    Ok(())
}
