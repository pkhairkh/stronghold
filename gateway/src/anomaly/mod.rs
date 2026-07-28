//! Anomaly detection — scan PTY stream for suspicious patterns.
//!
//! Does NOT block execution (agent has full access during session).
//! Pushes the tenant's phone for review when patterns match.

use regex::Regex;
use std::sync::Arc;

/// Anomaly patterns loaded from `anomaly.toml`.
pub struct AnomalyScanner {
    patterns: Vec<AnomalyPattern>,
}

#[derive(Debug)]
pub struct AnomalyPattern {
    pub regex: Regex,
    pub message: String,
    pub push: bool,
}

#[derive(Debug, serde::Deserialize)]
struct AnomalyConfig {
    #[serde(default)]
    patterns: Vec<AnomalyConfigPattern>,
}

#[derive(Debug, serde::Deserialize)]
struct AnomalyConfigPattern {
    pattern: String,
    message: String,
    #[serde(default = "default_true")]
    push: bool,
}

fn default_true() -> bool {
    true
}

impl AnomalyScanner {
    /// Load patterns from a TOML file.
    pub fn from_config(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: AnomalyConfig = toml::from_str(&content)?;

        let patterns = config
            .patterns
            .into_iter()
            .filter_map(|p| {
                Regex::new(&p.pattern)
                    .ok()
                    .map(|regex| AnomalyPattern {
                        regex,
                        message: p.message,
                        push: p.push,
                    })
            })
            .collect();

        Ok(Self { patterns })
    }

    /// Scan a command for anomalies.
    pub fn scan(&self, cmd: &str) -> Vec<&AnomalyPattern> {
        self.patterns
            .iter()
            .filter(|p| p.regex.is_match(cmd))
            .collect()
    }

    /// Get default patterns (used when no config file is loaded).
    pub fn defaults() -> Self {
        let patterns = vec![
            AnomalyPattern {
                regex: Regex::new(r"curl|wget|scp").unwrap(),
                message: "agent exfiltrating to external host".to_string(),
                push: true,
            },
            AnomalyPattern {
                regex: Regex::new(r"rm -rf (?!/tmp/|target/)").unwrap(),
                message: "destructive rm outside allowed paths".to_string(),
                push: true,
            },
            AnomalyPattern {
                regex: Regex::new(r"sudo (?!cargo|lake|qemu|wasmtime)").unwrap(),
                message: "privilege escalation attempt".to_string(),
                push: true,
            },
            AnomalyPattern {
                regex: Regex::new(r"ssh\s+\S+@").unwrap(),
                message: "agent SSHing to external host".to_string(),
                push: true,
            },
        ];

        Self { patterns }
    }
}
