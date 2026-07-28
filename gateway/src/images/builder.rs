//! Image builder — generate Containerfiles from image.toml and build OCI images.

use anyhow::Result;
use crate::images::dsl::ImageConfig;

/// Generate a Containerfile from an image.toml config.
pub fn generate_containerfile(config: &ImageConfig) -> Result<String> {
    let mut lines = Vec::new();

    // FROM
    lines.push(format!("FROM stronghold/{}", config.extends));

    // Labels
    if !config.labels.is_empty() {
        lines.push(String::new());
        for (key, value) in &config.labels {
            lines.push(format!("LABEL {}=\"{}\"", key, value));
        }
    }

    // Pre-install scripts
    if !config.pre_install.commands.is_empty() {
        lines.push(String::new());
        lines.push("RUN".to_string());
        for cmd in &config.pre_install.commands {
            lines.push(format!("    && {}", cmd));
        }
    }

    // Packages (dnf)
    if !config.packages.dnf.is_empty() {
        lines.push(String::new());
        let pkgs = config.packages.dnf.join(" ");
        lines.push(format!("RUN dnf install -y {}", pkgs));
    }

    // Toolchains
    for toolchain in config.toolchains.values() {
        lines.push(String::new());
        match toolchain {
            crate::images::dsl::Toolchain::Rust { channel, date, targets, components } => {
                lines.push(format!("# Toolchain: rust ({})", channel));
                lines.push("RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y".to_string());
                lines.push("ENV PATH=\"/root/.cargo/bin:${PATH}\"".to_string());
                if let Some(d) = date {
                    lines.push(format!("RUN rustup toolchain install {}-{} --profile minimal", channel, d));
                    lines.push(format!("RUN rustup default {}-{}", channel, d));
                } else {
                    lines.push(format!("RUN rustup default {}", channel));
                }
                if !targets.is_empty() {
                    for target in targets {
                        lines.push(format!("RUN rustup target add {}", target));
                    }
                }
                if !components.is_empty() {
                    for comp in components {
                        lines.push(format!("RUN rustup component add {}", comp));
                    }
                }
            }
            crate::images::dsl::Toolchain::Node { version } => {
                lines.push(format!("# Toolchain: node ({})", version));
                lines.push(format!("RUN curl -fsSL https://rpm.nodesource.com/setup_{}.x | bash -", version));
                lines.push("RUN dnf install -y nodejs".to_string());
                lines.push("RUN npm install -g pnpm".to_string());
            }
            crate::images::dsl::Toolchain::Python { version } => {
                lines.push(format!("# Toolchain: python ({})", version));
                lines.push(format!("RUN dnf install -y python{} python{}-pip", version, version));
            }
            crate::images::dsl::Toolchain::Go { version } => {
                lines.push(format!("# Toolchain: go ({})", version));
                lines.push(format!("RUN curl -fsSL https://go.dev/dl/go{}.linux-amd64.tar.gz | tar -C /usr/local -xz", version));
                lines.push("ENV PATH=\"/usr/local/go/bin:${PATH}\"".to_string());
            }
            crate::images::dsl::Toolchain::Elan { channel, date } => {
                lines.push(format!("# Toolchain: elan ({})", channel));
                lines.push("RUN curl -fsSL https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh | sh -s -- -y".to_string());
                lines.push("ENV PATH=\"/root/.elan/bin:${PATH}\"".to_string());
                let _ = date; // TODO: pin elan to specific date
            }
        }
    }

    // Environment variables
    if !config.env.is_empty() {
        lines.push(String::new());
        for (key, value) in &config.env {
            lines.push(format!("ENV {}=\"{}\"", key, value));
        }
    }

    // Post-install scripts
    if !config.post_install.commands.is_empty() {
        lines.push(String::new());
        lines.push("RUN".to_string());
        for cmd in &config.post_install.commands {
            lines.push(format!("    && {}", cmd));
        }
    }

    // Injected Containerfile snippets (escape hatch)
    if !config.inject_containerfile.snippets.is_empty() {
        lines.push(String::new());
        lines.push("# Injected snippets (escape hatch)".to_string());
        for snippet in &config.inject_containerfile.snippets {
            lines.push(snippet.clone());
        }
    }

    Ok(lines.join("\n"))
}

/// Build an OCI image from an image.toml config.
pub async fn build(config: &ImageConfig, tag: &str) -> Result<String> {
    let containerfile = generate_containerfile(config)?;

    tracing::info!(
        image = %config.name,
        tag = %tag,
        "Building OCI image (stub)"
    );

    // Write Containerfile to temp dir
    let temp_dir = std::env::temp_dir().join(format!("stronghold-build-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&temp_dir)?;
    std::fs::write(temp_dir.join("Containerfile"), &containerfile)?;

    // TODO: call podman or docker build
    // For now, just log the generated Containerfile
    tracing::debug!("Generated Containerfile:\n{}", containerfile);

    // Return the image digest (stub)
    let digest = format!("sha256:{}", hex::encode([0u8; 32]));
    Ok(digest)
}
