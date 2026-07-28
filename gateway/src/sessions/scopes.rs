//! Scope definitions — what requires how many credentials.
//!
//! Per-tenant `scopes.toml` defines:
//! - `default` scope: full PTY, single credential, TTL'd
//! - `extended` scope: full PTY, single credential, longer TTL
//! - `destructive` scope: quorum (2+ credentials), short TTL, pattern-matched

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeConfig {
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    pub name: String,
    pub shell: String,
    pub patterns: Vec<String>,
    pub require_credentials: u32,
    pub ttl_secs: u64,
}

impl Default for ScopeConfig {
    fn default() -> Self {
        ScopeConfig {
            scopes: vec![
                Scope {
                    name: "default".to_string(),
                    shell: "full PTY".to_string(),
                    patterns: vec![],
                    require_credentials: 1,
                    ttl_secs: 14400,
                },
                Scope {
                    name: "extended".to_string(),
                    shell: "full PTY".to_string(),
                    patterns: vec![],
                    require_credentials: 1,
                    ttl_secs: 28800,
                },
                Scope {
                    name: "destructive".to_string(),
                    shell: "full PTY".to_string(),
                    patterns: vec![
                        "rm -rf".to_string(),
                        "git push --force".to_string(),
                        "DROP TABLE".to_string(),
                        "sudo rm".to_string(),
                    ],
                    require_credentials: 2,
                    ttl_secs: 1800,
                },
            ],
        }
    }
}

/// Check if a command matches a destructive scope pattern.
/// If so, quorum re-approval is required mid-session.
pub fn matches_deceptive_pattern<'a>(
    config: &'a ScopeConfig,
    cmd: &str,
) -> Option<&'a Scope> {
    for scope in &config.scopes {
        if scope.name == "destructive" {
            for pattern in &scope.patterns {
                if cmd.contains(pattern) {
                    return Some(scope);
                }
            }
        }
    }
    None
}

/// Load scope config from a TOML file.
pub fn load(path: &str) -> anyhow::Result<ScopeConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: ScopeConfig = toml::from_str(&content)?;
    Ok(config)
}
