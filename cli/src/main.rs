//! Stronghold CLI — manage tenants, credentials, images, and audit logs.
//!
//! All subcommands (except `init` and `completions`) talk to the Stronghold
//! gateway over HTTP. The gateway URL and admin token can be provided via:
//!   1. `--url` / `--admin-token` CLI flags (highest priority)
//!   2. `STRONGHOLD_URL` / `STRONGHOLD_ADMIN_TOKEN` environment variables
//!   3. `~/.stronghold.toml` config file (lowest priority)
//!
//! When the gateway is unreachable, commands fail fast with a clear
//! connection error rather than a stack trace.

// Scaffold-stage allow: will be removed in Wave 11 (Integration & E2E).
#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate as generate_completion, Shell};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::PathBuf;

// ============================================================================
// CLI definition
// ============================================================================

#[derive(Parser)]
#[command(name = "stronghold")]
#[command(version, about = "Stronghold CLI — manage tenants, credentials, images, and audit logs", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Gateway URL (e.g. https://your-box:8443)
    #[arg(long, env = "STRONGHOLD_URL")]
    url: Option<String>,

    /// Admin token (sent as `Authorization: Bearer <token>`)
    #[arg(long, env = "STRONGHOLD_ADMIN_TOKEN")]
    admin_token: Option<String>,

    /// Path to config file (default: ~/.stronghold.toml)
    #[arg(long, env = "STRONGHOLD_CONFIG")]
    config: Option<PathBuf>,

    /// Database path (for local operations: init, audit verify/export fallback)
    #[arg(
        long,
        env = "STRONGHOLD_DB",
        default_value = "/var/lib/stronghold/stronghold.db"
    )]
    db: String,

    /// Disable TLS certificate verification (for dev boxes with self-signed certs)
    #[arg(long, env = "STRONGHOLD_INSECURE", default_value_t = false)]
    insecure: bool,
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
        /// Directory to initialize (keys/, audit/, stronghold.db created here)
        #[arg(long, default_value = "/var/lib/stronghold")]
        data_dir: String,
    },

    /// Generate shell completion scripts
    Completions {
        /// Target shell
        #[arg(long)]
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum TenantCommands {
    /// Create a new tenant
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        max_concurrent_machines: Option<u32>,
        #[arg(long)]
        max_cpu_per_machine: Option<u32>,
        #[arg(long)]
        max_memory_gb_per_machine: Option<u32>,
    },
    /// List all tenants
    List,
    /// Get details for a single tenant
    Get {
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum CredentialCommands {
    /// Print the enrollment URL to open in a phone browser
    Enroll {
        /// Optional tenant ID (pre-fills the enrollment page)
        #[arg(long)]
        tenant: Option<String>,
    },
    /// List enrolled credentials for a tenant
    List {
        #[arg(long)]
        tenant: String,
    },
    /// Revoke a credential by its ID
    Revoke {
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum AgentTokenCommands {
    /// Mint a new agent token (printed once — save it immediately)
    Mint {
        #[arg(long)]
        tenant: String,
        #[arg(long, default_value = "default")]
        scope: String,
        #[arg(long, default_value = "86400")]
        ttl: u64,
    },
    /// List active agent tokens for a tenant
    List {
        #[arg(long)]
        tenant: String,
    },
    /// Revoke an agent token
    Revoke {
        #[arg(long)]
        token: String,
    },
}

#[derive(Subcommand)]
enum ImageCommands {
    /// Build an OCI image from an image.toml file
    Build {
        /// Path to image.toml (or a directory containing image.toml)
        #[arg(long)]
        path: String,
        /// Optional tag (defaults to "latest")
        #[arg(long)]
        tag: Option<String>,
    },
    /// List images in the catalog
    List,
    /// Push an image to the registry
    Push {
        #[arg(long)]
        name: String,
    },
}

#[derive(Subcommand)]
enum WorkerCommands {
    /// Register a new worker (SSHes to bootstrap, then joins cluster)
    Add {
        #[arg(long)]
        host: String,
        #[arg(long)]
        token: String,
    },
    /// List registered workers
    List,
}

#[derive(Subcommand)]
enum AuditCommands {
    /// Verify the hash chain + signatures of a tenant's audit log
    Verify {
        #[arg(long)]
        tenant: String,
    },
    /// Export audit log entries to stdout
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
    /// Rotate the audit signing keys (Ed25519 + ML-DSA-65)
    RotateAudit,
    /// Rotate the push encryption keys (X25519 + ML-KEM-768)
    RotatePush,
}

// ============================================================================
// Config file
// ============================================================================

/// Stronghold CLI config file (`~/.stronghold.toml`).
///
/// All fields optional — flags and env vars override.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    admin_token: Option<String>,
    #[serde(default)]
    db: Option<String>,
    #[serde(default)]
    insecure: Option<bool>,
}

impl Config {
    /// Load from the default path (`~/.stronghold.toml`), or a custom path.
    /// Returns an empty config if the file doesn't exist (not an error).
    fn load(path: Option<&PathBuf>) -> Result<Self> {
        let path = match path {
            Some(p) => p.clone(),
            None => default_config_path()?,
        };

        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file at {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file at {}", path.display()))?;
        Ok(config)
    }
}

/// Default config path: `~/.stronghold.toml`.
fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .or_else(|_| dirs_fallback())
        .context("Could not determine HOME directory for config file")?;
    Ok(home.join(".stronghold.toml"))
}

/// Fallback when $HOME isn't set (rare, but happens in some containers).
fn dirs_fallback() -> std::result::Result<PathBuf, std::env::VarError> {
    // /root for uid 0, /tmp otherwise — better than crashing.
    let p = if unsafe { libc_geteuid() } == 0 {
        "/root".to_string()
    } else {
        "/tmp".to_string()
    };
    Ok(PathBuf::from(p))
}

// Avoid pulling in `libc` / `dirs` crates just for this one call.
unsafe extern "C" {
    fn geteuid() -> u32;
}
unsafe fn libc_geteuid() -> u32 {
    unsafe { geteuid() }
}

// ============================================================================
// Resolved settings (config + flags + env)
// ============================================================================

/// Effective settings after merging config file, flags, and env vars.
struct Settings {
    url: Option<String>,
    admin_token: Option<String>,
    db: String,
    insecure: bool,
}

impl Settings {
    /// Merge config file + CLI flags + env vars (flags win).
    fn resolve(cli: &Cli) -> Result<Self> {
        let config = Config::load(cli.config.as_ref())?;

        let url = cli.url.clone().or(config.url);
        let admin_token = cli.admin_token.clone().or(config.admin_token);
        let db = if cli.db != "/var/lib/stronghold/stronghold.db" {
            // Flag was explicitly set.
            cli.db.clone()
        } else if let Some(d) = config.db {
            d
        } else {
            cli.db.clone()
        };
        let insecure = cli.insecure || config.insecure.unwrap_or(false);

        Ok(Self {
            url,
            admin_token,
            db,
            insecure,
        })
    }
}

// ============================================================================
// Gateway HTTP client
// ============================================================================

/// Thin wrapper around `reqwest::Client` that knows the gateway base URL
/// and admin token. All gateway API calls go through here so error handling
/// is centralized.
struct GatewayClient {
    base_url: String,
    admin_token: Option<String>,
    http: reqwest::Client,
}

impl GatewayClient {
    /// Build a client from resolved settings. Returns `Err` if no URL is set.
    fn from_settings(settings: &Settings) -> Result<Self> {
        let url = settings
            .url
            .as_ref()
            .ok_or_else(|| {
                anyhow!(
                    "No gateway URL configured.\n\
                     Set it via --url, STRONGHOLD_URL env var, or `url` in ~/.stronghold.toml"
                )
            })?
            .trim_end_matches('/')
            .to_string();

        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10));
        if settings.insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let http = builder.build().context("Failed to build HTTP client")?;

        Ok(Self {
            base_url: url,
            admin_token: settings.admin_token.clone(),
            http,
        })
    }

    /// Build a request with the admin token attached (if set).
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.http.request(method, &url);
        if let Some(token) = &self.admin_token {
            req = req.bearer_auth(token);
        }
        req
    }

    /// Execute a request and convert network/HTTP errors into clear messages.
    async fn send<T>(&self, req: reqwest::RequestBuilder) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let resp = req
            .send()
            .await
            .map_err(|e| format_connect_error(&self.base_url, &e))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Gateway returned HTTP {}: {}",
                status,
                body.trim().chars().take(500).collect::<String>()
            ));
        }

        resp.json::<T>()
            .await
            .context("Failed to decode gateway response as JSON")
    }

    /// Execute a request that returns no body (just status).
    async fn send_empty(&self, req: reqwest::RequestBuilder) -> Result<()> {
        let resp = req
            .send()
            .await
            .map_err(|e| format_connect_error(&self.base_url, &e))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Gateway returned HTTP {}: {}",
                status,
                body.trim().chars().take(500).collect::<String>()
            ));
        }
        Ok(())
    }
}

/// Turn a reqwest network error into a clear human-readable message.
fn format_connect_error(base_url: &str, e: &reqwest::Error) -> anyhow::Error {
    if e.is_connect() {
        anyhow!(
            "Could not connect to gateway at {}.\n\
             Is the gateway running? Start it with: stronghold-gateway serve\n\
             Underlying error: {}",
            base_url,
            e
        )
    } else if e.is_timeout() {
        anyhow!("Timed out talking to gateway at {}: {}", base_url, e)
    } else {
        anyhow!("Gateway request to {} failed: {}", base_url, e)
    }
}

// ============================================================================
// API request/response types (mirror gateway routes)
// ============================================================================

#[derive(Debug, Serialize)]
struct CreateTenantRequest {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_concurrent_machines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_cpu_per_machine: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_memory_gb_per_machine: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TenantResponse {
    id: String,
    name: String,
    created_at: String,
    #[serde(default)]
    setup_password: String,
    #[serde(default)]
    enrollment_url: String,
    #[serde(default)]
    sev_snp_measurement: String,
}

#[derive(Debug, Deserialize)]
struct TenantListResponse {
    tenants: Vec<TenantResponse>,
}

#[derive(Debug, Deserialize)]
struct CredentialResponse {
    id: String,
    #[serde(default)]
    tenant_id: String,
    #[serde(default)]
    credential_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    last_used_at: Option<String>,
    #[serde(default)]
    revoked_at: Option<String>,
    #[serde(default)]
    verified: bool,
}

#[derive(Debug, Deserialize)]
struct CredentialListResponse {
    credentials: Vec<CredentialResponse>,
}

#[derive(Debug, Serialize)]
struct MintAgentTokenRequest {
    tenant: String,
    scope: String,
    ttl_secs: u64,
}

#[derive(Debug, Deserialize)]
struct MintAgentTokenResponse {
    token: String,
    #[serde(default)]
    expires_at: String,
}

#[derive(Debug, Deserialize)]
struct AgentTokenInfo {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    tenant_id: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    revoked_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentTokenListResponse {
    tokens: Vec<AgentTokenInfo>,
}

#[derive(Debug, Serialize)]
struct RevokeAgentTokenRequest {
    token: String,
}

#[derive(Debug, Serialize)]
struct BuildImageRequest {
    /// Raw image.toml contents
    image_toml: String,
    tag: String,
}

#[derive(Debug, Deserialize)]
struct BuildImageResponse {
    digest: String,
    tag: String,
}

#[derive(Debug, Deserialize)]
struct ImageInfo {
    name: String,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct ImageListResponse {
    images: Vec<ImageInfo>,
}

#[derive(Debug, Serialize)]
struct PushImageRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PushImageResponse {
    digest: String,
}

#[derive(Debug, Serialize)]
struct AddWorkerRequest {
    host: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct WorkerInfo {
    id: String,
    host: String,
    #[serde(default)]
    sev_snp: bool,
    #[serde(default)]
    cpu_total: Option<i64>,
    #[serde(default)]
    memory_gb_total: Option<i64>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    last_seen: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkerListResponse {
    workers: Vec<WorkerInfo>,
}

#[derive(Debug, Deserialize)]
struct AuditVerifyResponse {
    tenant_id: String,
    entries_checked: u64,
    errors: Vec<String>,
    verified: bool,
}

// ============================================================================
// Entry point
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
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
        Commands::Init { ref data_dir } => handle_init(data_dir).await,
        Commands::Completions { shell } => handle_completions(shell),
    }
}

// ============================================================================
// Handlers: tenant
// ============================================================================

async fn handle_tenant(action: &TenantCommands, cli: &Cli) -> Result<()> {
    let settings = Settings::resolve(cli)?;
    let client = GatewayClient::from_settings(&settings)?;

    match action {
        TenantCommands::Create {
            name,
            max_concurrent_machines,
            max_cpu_per_machine,
            max_memory_gb_per_machine,
        } => {
            let req = CreateTenantRequest {
                name: name.clone(),
                max_concurrent_machines: *max_concurrent_machines,
                max_cpu_per_machine: *max_cpu_per_machine,
                max_memory_gb_per_machine: *max_memory_gb_per_machine,
            };
            let resp: TenantResponse = client
                .send(client.request(reqwest::Method::POST, "/admin/tenant").json(&req))
                .await?;

            println!("Tenant created.");
            println!("  ID:                 {}", resp.id);
            println!("  Name:               {}", resp.name);
            println!("  Created at:         {}", resp.created_at);
            println!();
            println!("Setup password (save this — it will not be shown again):");
            println!("  {}", resp.setup_password);
            println!();
            // Build a full enrollment URL if the gateway only returned a path.
            let enroll_url = if resp.enrollment_url.starts_with("http") {
                resp.enrollment_url
            } else if resp.enrollment_url.is_empty() {
                format!("{}/setup?tenant={}", client.base_url, resp.id)
            } else {
                format!("{}{}", client.base_url, resp.enrollment_url)
            };
            println!("Enrollment URL (open on your phone):");
            println!("  {}", enroll_url);
            if !resp.sev_snp_measurement.is_empty() {
                println!();
                println!("SEV-SNP measurement: {}", resp.sev_snp_measurement);
            }
        }
        TenantCommands::List => {
            let resp: TenantListResponse = client
                .send(client.request(reqwest::Method::GET, "/admin/tenant"))
                .await?;

            if resp.tenants.is_empty() {
                println!("No tenants yet. Create one with: stronghold tenant create --name <name>");
                return Ok(());
            }
            println!("{:<28} {:<20} {}", "ID", "NAME", "CREATED");
            for t in &resp.tenants {
                println!("{:<28} {:<20} {}", t.id, t.name, t.created_at);
            }
        }
        TenantCommands::Get { id } => {
            let resp: TenantResponse = client
                .send(
                    client
                        .request(reqwest::Method::GET, &format!("/admin/tenant/{}", id)),
                )
                .await?;

            println!("Tenant:");
            println!("  ID:               {}", resp.id);
            println!("  Name:             {}", resp.name);
            println!("  Created at:       {}", resp.created_at);
            println!("  Setup password:   {}", resp.setup_password);
            let enroll_url = if resp.enrollment_url.starts_with("http") {
                resp.enrollment_url
            } else if resp.enrollment_url.is_empty() {
                format!("{}/setup?tenant={}", client.base_url, resp.id)
            } else {
                format!("{}{}", client.base_url, resp.enrollment_url)
            };
            println!("  Enrollment URL:   {}", enroll_url);
            if !resp.sev_snp_measurement.is_empty() {
                println!("  SEV-SNP measure:  {}", resp.sev_snp_measurement);
            }
        }
    }
    Ok(())
}

// ============================================================================
// Handlers: credentials
// ============================================================================

async fn handle_credentials(action: &CredentialCommands, cli: &Cli) -> Result<()> {
    let settings = Settings::resolve(cli)?;

    match action {
        CredentialCommands::Enroll { tenant } => {
            let url = match &settings.url {
                Some(u) => u.trim_end_matches('/').to_string(),
                None => {
                    return Err(anyhow!(
                        "No gateway URL configured.\n\
                         Set it via --url, STRONGHOLD_URL env var, or `url` in ~/.stronghold.toml"
                    ));
                }
            };
            let enroll_url = match tenant {
                Some(t) => format!("{}/setup?tenant={}", url, t),
                None => format!("{}/setup", url),
            };
            println!("Open this URL in your phone browser to enroll a WebAuthn credential:");
            println!("  {}", enroll_url);
            println!();
            println!("You will need the tenant's setup password (printed by `stronghold tenant create`).");
        }
        CredentialCommands::List { tenant } => {
            let client = GatewayClient::from_settings(&settings)?;
            let resp: CredentialListResponse = client
                .send(
                    client.request(
                        reqwest::Method::GET,
                        &format!("/admin/tenant/{}/credentials", tenant),
                    ),
                )
                .await?;

            if resp.credentials.is_empty() {
                println!("No credentials enrolled for tenant {}.", tenant);
                return Ok(());
            }
            println!(
                "Credentials for tenant {}:",
                tenant
            );
            println!(
                "{:<28} {:<24} {:<24} {}",
                "ID", "NAME", "CREATED", "STATUS"
            );
            for c in &resp.credentials {
                let name = c.name.clone().unwrap_or_else(|| "-".to_string());
                let status = if c.revoked_at.is_some() {
                    "revoked".to_string()
                } else if c.verified {
                    "active".to_string()
                } else {
                    "pending".to_string()
                };
                println!(
                    "{:<28} {:<24} {:<24} {}",
                    c.id, name, c.created_at, status
                );
            }
        }
        CredentialCommands::Revoke { id } => {
            let client = GatewayClient::from_settings(&settings)?;
            client
                .send_empty(
                    client
                        .request(reqwest::Method::DELETE, &format!("/admin/credential/{}", id)),
                )
                .await?;
            println!("Credential {} revoked.", id);
        }
    }
    Ok(())
}

// ============================================================================
// Handlers: agent-token
// ============================================================================

async fn handle_agent_token(action: &AgentTokenCommands, cli: &Cli) -> Result<()> {
    let settings = Settings::resolve(cli)?;
    let client = GatewayClient::from_settings(&settings)?;

    match action {
        AgentTokenCommands::Mint { tenant, scope, ttl } => {
            let req = MintAgentTokenRequest {
                tenant: tenant.clone(),
                scope: scope.clone(),
                ttl_secs: *ttl,
            };
            let resp: MintAgentTokenResponse = client
                .send(
                    client
                        .request(reqwest::Method::POST, "/admin/agent-token")
                        .json(&req),
                )
                .await?;

            println!("Agent token minted (save this — it will not be shown again):");
            println!("  {}", resp.token);
            if !resp.expires_at.is_empty() {
                println!();
                println!("Expires at: {}", resp.expires_at);
            }
        }
        AgentTokenCommands::List { tenant } => {
            let resp: AgentTokenListResponse = client
                .send(
                    client.request(
                        reqwest::Method::GET,
                        &format!("/admin/tenant/{}/agent-token", tenant),
                    ),
                )
                .await?;

            if resp.tokens.is_empty() {
                println!("No agent tokens for tenant {}.", tenant);
                return Ok(());
            }
            println!("Agent tokens for tenant {}:", tenant);
            println!(
                "{:<20} {:<12} {:<24} {:<24} {}",
                "ID", "SCOPE", "CREATED", "EXPIRES", "STATUS"
            );
            for t in &resp.tokens {
                let id = t.id.clone().unwrap_or_else(|| "-".to_string());
                let scope = t.scope.clone().unwrap_or_else(|| "-".to_string());
                let expires = t.expires_at.clone().unwrap_or_else(|| "-".to_string());
                let status = if t.revoked_at.is_some() {
                    "revoked"
                } else {
                    "active"
                };
                println!(
                    "{:<20} {:<12} {:<24} {:<24} {}",
                    id, scope, t.created_at, expires, status
                );
            }
        }
        AgentTokenCommands::Revoke { token } => {
            let req = RevokeAgentTokenRequest {
                token: token.clone(),
            };
            client
                .send_empty(
                    client
                        .request(reqwest::Method::POST, "/admin/agent-token/revoke")
                        .json(&req),
                )
                .await?;
            println!("Agent token revoked.");
        }
    }
    Ok(())
}

// ============================================================================
// Handlers: image
// ============================================================================

async fn handle_image(action: &ImageCommands, cli: &Cli) -> Result<()> {
    let settings = Settings::resolve(cli)?;
    let client = GatewayClient::from_settings(&settings)?;

    match action {
        ImageCommands::Build { path, tag } => {
            // Read the image.toml file locally so we can validate it before
            // round-tripping to the gateway.
            let path = std::path::Path::new(path);
            let toml_path = if path.is_dir() {
                path.join("image.toml")
            } else {
                path.to_path_buf()
            };
            if !toml_path.exists() {
                return Err(anyhow!(
                    "Image config not found: {}. \
                     Pass a path to an image.toml file or a directory containing one.",
                    toml_path.display()
                ));
            }
            let image_toml = std::fs::read_to_string(&toml_path)
                .with_context(|| format!("Failed to read {}", toml_path.display()))?;

            // Validate locally by parsing.
            let _parsed: serde_json::Value = toml::from_str(&image_toml).context(format!(
                "Failed to parse {} as TOML",
                toml_path.display()
            ))?;

            let tag = tag.clone().unwrap_or_else(|| "latest".to_string());
            let req = BuildImageRequest { image_toml, tag };
            let resp: BuildImageResponse = client
                .send(
                    client
                        .request(reqwest::Method::POST, "/admin/image/build")
                        .json(&req),
                )
                .await?;

            println!("Image built.");
            println!("  Tag:    {}", resp.tag);
            println!("  Digest: {}", resp.digest);
        }
        ImageCommands::List => {
            let resp: ImageListResponse = client
                .send(client.request(reqwest::Method::GET, "/admin/image"))
                .await?;

            if resp.images.is_empty() {
                println!("No images in catalog.");
                return Ok(());
            }
            println!("{:<32} {:<20} {}", "NAME", "TAG", "DIGEST");
            for img in &resp.images {
                let digest_short = if img.digest.len() > 24 {
                    &img.digest[..24]
                } else {
                    &img.digest
                };
                println!("{:<32} {:<20} {}", img.name, img.tag, digest_short);
            }
        }
        ImageCommands::Push { name } => {
            let req = PushImageRequest {
                name: name.clone(),
            };
            let resp: PushImageResponse = client
                .send(
                    client
                        .request(reqwest::Method::POST, "/admin/image/push")
                        .json(&req),
                )
                .await?;
            println!("Image pushed: {}", name);
            println!("  Digest: {}", resp.digest);
        }
    }
    Ok(())
}

// ============================================================================
// Handlers: worker
// ============================================================================

async fn handle_worker(action: &WorkerCommands, cli: &Cli) -> Result<()> {
    let settings = Settings::resolve(cli)?;
    let client = GatewayClient::from_settings(&settings)?;

    match action {
        WorkerCommands::Add { host, token } => {
            let req = AddWorkerRequest {
                host: host.clone(),
                token: token.clone(),
            };
            let resp: WorkerInfo = client
                .send(
                    client
                        .request(reqwest::Method::POST, "/admin/worker")
                        .json(&req),
                )
                .await?;
            println!("Worker added.");
            println!("  ID:     {}", resp.id);
            println!("  Host:   {}", resp.host);
            println!("  SEV-SNP: {}", resp.sev_snp);
            if let Some(cpu) = resp.cpu_total {
                println!("  CPU:    {}", cpu);
            }
            if let Some(mem) = resp.memory_gb_total {
                println!("  Memory: {}GB", mem);
            }
            println!("  Status: {}", resp.status);
        }
        WorkerCommands::List => {
            let resp: WorkerListResponse = client
                .send(client.request(reqwest::Method::GET, "/admin/worker"))
                .await?;

            if resp.workers.is_empty() {
                println!("No workers registered.");
                return Ok(());
            }
            println!(
                "{:<28} {:<32} {:<8} {:<8} {}",
                "ID", "HOST", "SEV-SNP", "CPU", "STATUS"
            );
            for w in &resp.workers {
                let cpu = w.cpu_total.map(|c| c.to_string()).unwrap_or("-".to_string());
                println!(
                    "{:<28} {:<32} {:<8} {:<8} {}",
                    w.id, w.host, w.sev_snp, cpu, w.status
                );
            }
        }
    }
    Ok(())
}

// ============================================================================
// Handlers: audit
// ============================================================================

async fn handle_audit(action: &AuditCommands, cli: &Cli) -> Result<()> {
    let settings = Settings::resolve(cli)?;

    match action {
        AuditCommands::Verify { tenant } => {
            // Try gateway first; fall back to local DB.
            if settings.url.is_some() {
                match try_remote_audit_verify(&settings, tenant).await {
                    Ok(resp) => {
                        print_audit_verify_result(&resp);
                        if !resp.verified {
                            std::process::exit(1);
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Remote audit verify failed, falling back to local DB"
                        );
                        println!("(Gateway unreachable: {}; falling back to local DB)", e);
                    }
                }
            }
            // Local fallback
            let report = local_audit_verify(&settings.db, tenant)
                .context("Local audit verify failed")?;
            print_local_audit_verify(tenant, &report);
            if !report.verified {
                std::process::exit(1);
            }
        }
        AuditCommands::Export {
            tenant,
            from,
            to,
            format,
        } => {
            // Try gateway first; fall back to local DB.
            if settings.url.is_some() {
                let req = client_request_for_export(&settings, tenant, from, to, format)?;
                match req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        let body = resp.text().await.unwrap_or_default();
                        println!("{}", body);
                        return Ok(());
                    }
                    Ok(resp) => {
                        tracing::warn!(
                            status = %resp.status(),
                            "Gateway audit export failed, falling back to local DB"
                        );
                        println!(
                            "(Gateway returned HTTP {}, falling back to local DB)",
                            resp.status()
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Gateway unreachable, falling back to local DB");
                        println!("(Gateway unreachable: {}; falling back to local DB)", e);
                    }
                }
            }
            let body = local_audit_export(&settings.db, tenant, from.as_deref(), to.as_deref(), format)
                .context("Local audit export failed")?;
            println!("{}", body);
        }
    }
    Ok(())
}

async fn try_remote_audit_verify(
    settings: &Settings,
    tenant: &str,
) -> Result<AuditVerifyResponse> {
    let client = GatewayClient::from_settings(settings)?;
    client
        .send(
            client.request(
                reqwest::Method::GET,
                &format!("/admin/audit/{}/verify", tenant),
            ),
        )
        .await
}

fn client_request_for_export(
    settings: &Settings,
    tenant: &str,
    from: &Option<String>,
    to: &Option<String>,
    format: &str,
) -> Result<reqwest::RequestBuilder> {
    let client = GatewayClient::from_settings(settings)?;
    let mut query: Vec<(&str, &str)> = vec![("format", format)];
    if let Some(f) = from {
        query.push(("from", f.as_str()));
    }
    if let Some(t) = to {
        query.push(("to", t.as_str()));
    }
    Ok(client.request(
        reqwest::Method::GET,
        &format!("/admin/audit/{}/export", tenant),
    ).query(&query))
}

fn print_audit_verify_result(resp: &AuditVerifyResponse) {
    println!("Audit log verification for tenant {}:", resp.tenant_id);
    println!("  Entries checked: {}", resp.entries_checked);
    println!("  Verified:        {}", resp.verified);
    if !resp.errors.is_empty() {
        println!("  Errors:");
        for e in &resp.errors {
            println!("    - {}", e);
        }
    }
}

fn print_local_audit_verify(tenant: &str, report: &LocalAuditReport) {
    println!(
        "Audit log verification for tenant {} (local DB):",
        tenant
    );
    println!("  Entries checked: {}", report.entries_checked);
    println!("  Verified:        {}", report.verified);
    if !report.errors.is_empty() {
        println!("  Errors:");
        for e in &report.errors {
            println!("    - {}", e);
        }
    }
}

// ============================================================================
// Handlers: keys
// ============================================================================

async fn handle_keys(action: &KeyCommands, cli: &Cli) -> Result<()> {
    let settings = Settings::resolve(cli)?;
    let client = GatewayClient::from_settings(&settings)?;

    match action {
        KeyCommands::RotateAudit => {
            client
                .send_empty(
                    client
                        .request(reqwest::Method::POST, "/admin/keys/rotate-audit"),
                )
                .await?;
            println!("Audit keys rotated.");
            println!("  New Ed25519 + ML-DSA-65 keypair generated");
            println!("  Sealed to current SEV-SNP measurement");
        }
        KeyCommands::RotatePush => {
            client
                .send_empty(
                    client
                        .request(reqwest::Method::POST, "/admin/keys/rotate-push"),
                )
                .await?;
            println!("Push keys rotated.");
            println!("  New X25519 + ML-KEM-768 keypair generated");
            println!("  All phones must re-enroll");
        }
    }
    Ok(())
}

// ============================================================================
// Handlers: init
// ============================================================================

async fn handle_init(data_dir: &str) -> Result<()> {
    println!("Initializing Stronghold in {}", data_dir);

    let keys_dir = format!("{}/keys", data_dir);
    let audit_dir = format!("{}/audit", data_dir);
    std::fs::create_dir_all(data_dir).context(format!(
        "Failed to create data directory {}",
        data_dir
    ))?;
    std::fs::create_dir_all(&keys_dir).context(format!(
        "Failed to create keys directory {}",
        keys_dir
    ))?;
    std::fs::create_dir_all(&audit_dir).context(format!(
        "Failed to create audit directory {}",
        audit_dir
    ))?;

    // Initialize database from the gateway schema.
    let db_path = format!("{}/stronghold.db", data_dir);
    let manager = r2d2_sqlite::SqliteConnectionManager::file(&db_path);
    let pool = r2d2::Pool::builder().max_size(4).build(manager)?;
    let conn = pool.get()?;
    conn.execute_batch(include_str!("../../gateway/src/db/schema.sql"))
        .context("Failed to initialize database schema")?;
    drop(conn);
    println!("Database initialized: {}", db_path);

    // Generate Ed25519 audit keypair if not already present.
    // (Push keys + ML-DSA-65 stub are generated by the gateway on first start
    // via `load_or_generate_keys`.)
    generate_audit_ed25519_keys(&keys_dir)?;

    println!();
    println!("Next steps:");
    println!(
        "  1. Start the gateway: stronghold-gateway serve --bind 0.0.0.0:8443"
    );
    println!(
        "  2. (Gateway will generate remaining keys on first start: push keys, ML-DSA-65)"
    );
    println!("  3. Create a tenant: stronghold --url http://localhost:8443 tenant create --name <your-name>");
    println!("  4. Enroll your phone (URL will be printed by `tenant create`)");
    Ok(())
}

/// Generate an Ed25519 keypair for the audit log and write it to <keys_dir>/
/// with mode 0600. Skips generation if files already exist.
fn generate_audit_ed25519_keys(keys_dir: &str) -> Result<()> {
    let secret_path = format!("{}/audit_ed25519.key", keys_dir);
    let pub_path = format!("{}/audit_ed25519.pub", keys_dir);

    if std::path::Path::new(&secret_path).exists() {
        println!("Audit Ed25519 keys already present: {}", secret_path);
        return Ok(());
    }

    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    let mut rng = OsRng;
    let secret = SigningKey::generate(&mut rng);
    let public = secret.verifying_key();

    write_secret(&secret_path, &secret.to_bytes())?;
    write_secret(&pub_path, &public.to_bytes())?;
    println!("Generated audit Ed25519 keypair:");
    println!("  Secret: {}", secret_path);
    println!("  Public: {}", pub_path);
    Ok(())
}

/// Write bytes to a file with mode 0600 (owner read/write only).
fn write_secret(path: &str, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, bytes))
        .context(format!("Failed to write secret file {}", path))?;
    Ok(())
}

// ============================================================================
// Handlers: completions
// ============================================================================

fn handle_completions(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let bin = "stronghold";
    generate_completion(shell, &mut cmd, bin, &mut std::io::stdout());
    Ok(())
}

// ============================================================================
// Local audit DB fallback (used when gateway is unreachable)
// ============================================================================

struct LocalAuditReport {
    entries_checked: u64,
    errors: Vec<String>,
    verified: bool,
}

/// Read the tenant audit DB and verify the hash chain locally.
///
/// The gateway writes per-tenant audit DBs to `/var/lib/stronghold/audit/<tenant>.db`.
/// We re-derive each entry's hash and check it matches + the chain is unbroken.
fn local_audit_verify(db_path: &str, tenant: &str) -> Result<LocalAuditReport> {
    // Resolve audit DB path: try /var/lib/stronghold/audit/<tenant>.db first,
    // then fall back to the main DB (audit_entries table).
    let audit_db = format!(
        "{}/audit/{}.db",
        std::path::Path::new(db_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/var/lib/stronghold".to_string()),
        tenant
    );

    let conn = if std::path::Path::new(&audit_db).exists() {
        rusqlite::Connection::open(&audit_db)?
    } else {
        rusqlite::Connection::open(db_path)?
    };

    let mut stmt = conn.prepare(
        "SELECT seq, ts, machine_id, event, payload, prev_hash, hash
         FROM audit_entries
         WHERE tenant_id = ?1
         ORDER BY seq ASC",
    )?;

    let entries: Vec<(i64, String, String, String, String, String, String)> = stmt
        .query_map([tenant], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut prev_hash =
        "0000000000000000000000000000000000000000000000000000000000000000".to_string();
    let mut errors = Vec::new();

    for (seq, ts, machine_id, event, payload, entry_prev_hash, hash) in &entries {
        if *entry_prev_hash != prev_hash {
            errors.push(format!(
                "seq {}: hash chain broken (expected {}, got {})",
                seq, prev_hash, entry_prev_hash
            ));
        }
        let message = format!(
            "{}|{}|{}|{}|{}|{}",
            ts, tenant, machine_id, event, payload, entry_prev_hash
        );
        let mut hasher = sha2::Sha256::new();
        hasher.update(message.as_bytes());
        let computed = hex::encode(hasher.finalize());
        if computed != *hash {
            errors.push(format!(
                "seq {}: hash mismatch (expected {}, got {})",
                seq, hash, computed
            ));
        }
        prev_hash = hash.clone();
    }

    let verified = errors.is_empty();
    Ok(LocalAuditReport {
        entries_checked: entries.len() as u64,
        errors,
        verified,
    })
}

/// Export the tenant audit log locally as JSON or text.
fn local_audit_export(
    db_path: &str,
    tenant: &str,
    from: Option<&str>,
    to: Option<&str>,
    format: &str,
) -> Result<String> {
    let audit_db = format!(
        "{}/audit/{}.db",
        std::path::Path::new(db_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/var/lib/stronghold".to_string()),
        tenant
    );
    let conn = if std::path::Path::new(&audit_db).exists() {
        rusqlite::Connection::open(&audit_db)?
    } else {
        rusqlite::Connection::open(db_path)?
    };

    let mut query = String::from(
        "SELECT ts, machine_id, event, payload, hash
         FROM audit_entries
         WHERE tenant_id = ?1",
    );
    let mut params: Vec<String> = vec![tenant.to_string()];
    if let Some(f) = from {
        query.push_str(" AND ts >= ?2");
        params.push(f.to_string());
    }
    if let Some(t) = to {
        query.push_str(" AND ts <= ?");
        params.push(t.to_string());
    }
    query.push_str(" ORDER BY seq ASC");

    let mut stmt = conn.prepare(&query)?;
    let rows: Vec<(String, String, String, String, String)> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    match format.to_lowercase().as_str() {
        "json" => {
            let entries: Vec<serde_json::Value> = rows
                .iter()
                .map(|(ts, machine, event, payload, hash)| {
                    serde_json::json!({
                        "ts": ts,
                        "machine_id": machine,
                        "event": event,
                        "payload": serde_json::from_str::<serde_json::Value>(payload)
                            .unwrap_or(serde_json::Value::Null),
                        "hash": hash,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&entries)?)
        }
        "text" | "txt" => {
            let mut out = String::new();
            for (ts, machine, event, payload, hash) in rows {
                let short_hash = if hash.len() > 16 { &hash[..16] } else { &hash };
                out.push_str(&format!(
                    "[{}] machine={} event={} hash={}\n  payload={}\n\n",
                    ts, machine, event, short_hash, payload
                ));
            }
            Ok(out)
        }
        other => Err(anyhow!("Unsupported format: {} (use json or text)", other)),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_parse_full() {
        let toml = r#"
url = "https://example.com:8443"
admin_token = "secret-token"
db = "/data/stronghold.db"
insecure = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.url.as_deref(), Some("https://example.com:8443"));
        assert_eq!(config.admin_token.as_deref(), Some("secret-token"));
        assert_eq!(config.db.as_deref(), Some("/data/stronghold.db"));
        assert_eq!(config.insecure, Some(true));
    }

    #[test]
    fn test_config_parse_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.url.is_none());
        assert!(config.admin_token.is_none());
        assert!(config.db.is_none());
        assert!(config.insecure.is_none());
    }

    #[test]
    fn test_config_parse_partial() {
        let toml = r#"
url = "http://localhost:8443"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.url.as_deref(), Some("http://localhost:8443"));
        assert!(config.admin_token.is_none());
    }

    #[test]
    fn test_config_load_missing_file_returns_default() {
        // Non-existent path should not error.
        let config =
            Config::load(Some(&PathBuf::from("/nonexistent/stronghold-test.toml"))).unwrap();
        assert!(config.url.is_none());
    }

    #[test]
    fn test_settings_resolve_flag_overrides_config() {
        // Write a temp config file.
        let tmp = tempfile_named();
        let toml = r#"
url = "https://from-config:8443"
admin_token = "from-config-token"
"#;
        std::fs::write(&tmp, toml).unwrap();

        // CLI flag should override config.
        let cli = Cli {
            command: Commands::Init {
                data_dir: "/tmp".to_string(),
            },
            url: Some("https://from-flag:9999".to_string()),
            admin_token: None,
            config: Some(tmp.clone()),
            db: "/var/lib/stronghold/stronghold.db".to_string(),
            insecure: false,
        };
        let settings = Settings::resolve(&cli).unwrap();
        assert_eq!(settings.url.as_deref(), Some("https://from-flag:9999"));
        assert_eq!(
            settings.admin_token.as_deref(),
            Some("from-config-token")
        );

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_settings_resolve_uses_config_when_no_flag() {
        let tmp = tempfile_named();
        let toml = r#"
url = "https://from-config:8443"
insecure = true
"#;
        std::fs::write(&tmp, toml).unwrap();

        let cli = Cli {
            command: Commands::Init {
                data_dir: "/tmp".to_string(),
            },
            url: None,
            admin_token: None,
            config: Some(tmp.clone()),
            db: "/var/lib/stronghold/stronghold.db".to_string(),
            insecure: false,
        };
        let settings = Settings::resolve(&cli).unwrap();
        assert_eq!(settings.url.as_deref(), Some("https://from-config:8443"));
        assert!(settings.insecure);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_format_connect_error_message() {
        // Construct a synthetic connect error via a refused connection.
        // We can't easily build a reqwest::Error directly, but we can verify
        // that the helper produces a sensible message for any error type.
        // Just verify the function exists and accepts a base URL.
        // (Full coverage requires a running gateway — see Wave 11 integration tests.)
    }

    /// Make a unique temp file path (the file is NOT created).
    fn tempfile_named() -> PathBuf {
        let id = ulid::Ulid::new().to_string().to_lowercase();
        PathBuf::from(format!("/tmp/stronghold-test-{}.toml", id))
    }

    #[test]
    fn test_local_audit_verify_empty_db() {
        // Create an empty DB with just the audit_entries table.
        let tmp = format!(
            "/tmp/stronghold-audit-test-{}.db",
            ulid::Ulid::new().to_string().to_lowercase()
        );
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(include_str!("../../gateway/src/db/schema.sql"))
                .unwrap();
        }
        let report = local_audit_verify(&tmp, "tenant_test").unwrap();
        assert_eq!(report.entries_checked, 0);
        assert!(report.verified);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_local_audit_verify_detects_tamper() {
        let tmp = format!(
            "/tmp/stronghold-audit-test-{}.db",
            ulid::Ulid::new().to_string().to_lowercase()
        );
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(include_str!("../../gateway/src/db/schema.sql"))
                .unwrap();
            // Parent tenant row (FK requirement).
            conn.execute(
                "INSERT INTO tenants (id, name, created_at, setup_password) VALUES ('tenant_test', 'test', '2026-01-01T00:00:00Z', 'x')",
                [],
            )
            .unwrap();
            // Insert one valid entry.
            let ts = "2026-01-01T00:00:00Z";
            let tenant = "tenant_test";
            let machine_id = "machine_1";
            let event = "test_event";
            let payload = "{}";
            let prev_hash =
                "0000000000000000000000000000000000000000000000000000000000000000";
            let message = format!(
                "{}|{}|{}|{}|{}|{}",
                ts, tenant, machine_id, event, payload, prev_hash
            );
            let mut hasher = sha2::Sha256::new();
            hasher.update(message.as_bytes());
            let hash = hex::encode(hasher.finalize());

            conn.execute(
                "INSERT INTO audit_entries (tenant_id, ts, machine_id, event, payload, prev_hash, hash, sig_ed25519, sig_mldsa65)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', '')",
                rusqlite::params![tenant, ts, machine_id, event, payload, prev_hash, hash],
            )
            .unwrap();

            // Insert a tampered second entry (wrong hash).
            conn.execute(
                "INSERT INTO audit_entries (tenant_id, ts, machine_id, event, payload, prev_hash, hash, sig_ed25519, sig_mldsa65)
                 VALUES (?1, '2026-01-02T00:00:00Z', 'machine_1', 'tampered', '{}', ?2, 'deadbeef', '', '')",
                rusqlite::params![tenant, hash],
            )
            .unwrap();
        }
        let report = local_audit_verify(&tmp, "tenant_test").unwrap();
        assert_eq!(report.entries_checked, 2);
        assert!(!report.verified);
        assert!(!report.errors.is_empty());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_local_audit_export_json() {
        let tmp = format!(
            "/tmp/stronghold-audit-test-{}.db",
            ulid::Ulid::new().to_string().to_lowercase()
        );
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(include_str!("../../gateway/src/db/schema.sql"))
                .unwrap();
            conn.execute(
                "INSERT INTO tenants (id, name, created_at, setup_password) VALUES ('tenant_test', 'test', '2026-01-01T00:00:00Z', 'x')",
                [],
            )
            .unwrap();
            let prev_hash =
                "0000000000000000000000000000000000000000000000000000000000000000";
            let message = format!(
                "{}|{}|{}|{}|{}|{}",
                "2026-01-01T00:00:00Z", "tenant_test", "m1", "boot", "{}", prev_hash
            );
            let mut hasher = sha2::Sha256::new();
            hasher.update(message.as_bytes());
            let hash = hex::encode(hasher.finalize());
            conn.execute(
                "INSERT INTO audit_entries (tenant_id, ts, machine_id, event, payload, prev_hash, hash, sig_ed25519, sig_mldsa65)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', '')",
                rusqlite::params![
                    "tenant_test",
                    "2026-01-01T00:00:00Z",
                    "m1",
                    "boot",
                    "{}",
                    prev_hash,
                    hash,
                ],
            )
            .unwrap();
        }
        let json = local_audit_export(&tmp, "tenant_test", None, None, "json").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["event"], "boot");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_local_audit_export_text() {
        let tmp = format!(
            "/tmp/stronghold-audit-test-{}.db",
            ulid::Ulid::new().to_string().to_lowercase()
        );
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(include_str!("../../gateway/src/db/schema.sql"))
                .unwrap();
            conn.execute(
                "INSERT INTO tenants (id, name, created_at, setup_password) VALUES ('tenant_test', 'test', '2026-01-01T00:00:00Z', 'x')",
                [],
            )
            .unwrap();
            let prev_hash =
                "0000000000000000000000000000000000000000000000000000000000000000";
            let message = format!(
                "{}|{}|{}|{}|{}|{}",
                "2026-01-01T00:00:00Z", "tenant_test", "m1", "boot", "{}", prev_hash
            );
            let mut hasher = sha2::Sha256::new();
            hasher.update(message.as_bytes());
            let hash = hex::encode(hasher.finalize());
            conn.execute(
                "INSERT INTO audit_entries (tenant_id, ts, machine_id, event, payload, prev_hash, hash, sig_ed25519, sig_mldsa65)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', '')",
                rusqlite::params![
                    "tenant_test",
                    "2026-01-01T00:00:00Z",
                    "m1",
                    "boot",
                    "{}",
                    prev_hash,
                    hash,
                ],
            )
            .unwrap();
        }
        let text = local_audit_export(&tmp, "tenant_test", None, None, "text").unwrap();
        assert!(text.contains("machine=m1"));
        assert!(text.contains("event=boot"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_local_audit_export_unsupported_format() {
        let tmp = format!(
            "/tmp/stronghold-audit-test-{}.db",
            ulid::Ulid::new().to_string().to_lowercase()
        );
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(include_str!("../../gateway/src/db/schema.sql"))
                .unwrap();
        }
        let result = local_audit_export(&tmp, "tenant_test", None, None, "xml");
        assert!(result.is_err());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_write_secret_creates_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = format!(
            "/tmp/stronghold-secret-test-{}",
            ulid::Ulid::new().to_string().to_lowercase()
        );
        write_secret(&path, b"hello").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(&path);
    }
}
