//! Image DSL parser — reads `image.toml` files and validates them.
//!
//! All images must `extends` from `stronghold/rocky-base`.

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageConfig {
    pub name: String,
    pub extends: String,
    #[serde(default)]
    pub description: String,

    #[serde(default)]
    pub packages: Packages,

    #[serde(default)]
    pub toolchains: std::collections::HashMap<String, Toolchain>,

    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,

    #[serde(default)]
    pub pre_install: ScriptSection,

    #[serde(default)]
    pub post_install: ScriptSection,

    #[serde(default)]
    pub inject_containerfile: InjectContainerfile,

    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Packages {
    #[serde(default)]
    pub dnf: Vec<String>,
    #[serde(default)]
    pub apt: Vec<String>, // alias for dnf on rocky; ignored
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Toolchain {
    Rust {
        channel: String,
        date: Option<String>,
        targets: Vec<String>,
        components: Vec<String>,
    },
    Node {
        version: String,
    },
    Python {
        version: String,
    },
    Go {
        version: String,
    },
    Elan {
        channel: String,
        date: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScriptSection {
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InjectContainerfile {
    #[serde(default)]
    pub snippets: Vec<String>,
}

/// Parse an `image.toml` file.
pub fn parse(content: &str) -> Result<ImageConfig> {
    let config: ImageConfig = toml::from_str(content)?;

    // Validate: must extend from rocky-base (directly or transitively)
    if config.extends != "rocky-base" && !config.extends.starts_with("stronghold/") {
        return Err(anyhow::anyhow!(
            "Image '{}' must extend from 'rocky-base' or a stronghold/* image (got: '{}')",
            config.name,
            config.extends
        ));
    }

    Ok(config)
}

/// Load an image config from a file path.
pub fn load(path: &str) -> Result<ImageConfig> {
    let content = std::fs::read_to_string(path)?;
    parse(&content)
}
