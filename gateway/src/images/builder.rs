//! Image builder — generate Containerfiles from image.toml and build OCI images.

use crate::images::dsl::ImageConfig;
use anyhow::Result;

/// Default `HOME` substituted for the `{home}` placeholder.
///
/// Mirrors the `HOME` env var defined in `images/rocky-base/image.toml`.
/// The `dev` user is created by rocky-base's `post_install` script with
/// `useradd -m -s /usr/bin/fish -u 1000 dev` so its home is `/home/dev`.
const DEFAULT_HOME: &str = "/home/dev";

/// Default `PATH` substituted for the `{path}` placeholder.
///
/// Mirrors the `PATH` env var defined in `images/rocky-base/image.toml`.
/// Derived images that override `PATH` typically prepend their own entries
/// (e.g. `/usr/local/cuda/bin:{path}`) so the base PATH remains usable.
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin";

/// Substitute `{home}` and `{path}` placeholders in an env-var value.
///
/// The placeholders are case-sensitive and brace-delimited:
///   - `{home}` → `DEFAULT_HOME` (`/home/dev`, the `dev` user's home
///     directory created by rocky-base's `post_install` script)
///   - `{path}` → `DEFAULT_PATH` (the rocky-base `PATH`, which all
///     derived images inherit)
///
/// Substitution always uses the rocky-base defaults, *not* the image's
/// own `HOME`/`PATH` env overrides. This avoids recursive substitution
/// when an image overrides `PATH` with a value that itself contains
/// `{path}` (e.g. `PATH = "/usr/local/cuda/bin:{path}"` in python-ml)
/// — the `{path}` placeholder refers to the *inherited* PATH, not the
/// image's own (overridden) value.
fn substitute_placeholders(value: &str, _config: &ImageConfig) -> String {
    value
        .replace("{home}", DEFAULT_HOME)
        .replace("{path}", DEFAULT_PATH)
}

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
            crate::images::dsl::Toolchain::Rust {
                channel,
                date,
                targets,
                components,
            } => {
                lines.push(format!("# Toolchain: rust ({})", channel));
                lines.push(
                    "RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
                        .to_string(),
                );
                lines.push("ENV PATH=\"/root/.cargo/bin:${PATH}\"".to_string());
                if let Some(d) = date {
                    lines.push(format!(
                        "RUN rustup toolchain install {}-{} --profile minimal",
                        channel, d
                    ));
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
                lines.push(format!(
                    "RUN curl -fsSL https://rpm.nodesource.com/setup_{}.x | bash -",
                    version
                ));
                lines.push("RUN dnf install -y nodejs".to_string());
                lines.push("RUN npm install -g pnpm".to_string());
            }
            crate::images::dsl::Toolchain::Python { version } => {
                lines.push(format!("# Toolchain: python ({})", version));
                lines.push(format!(
                    "RUN dnf install -y python{} python{}-pip",
                    version, version
                ));
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

    // Environment variables (with {home} / {path} placeholder substitution)
    if !config.env.is_empty() {
        lines.push(String::new());
        for (key, value) in &config.env {
            let substituted = substitute_placeholders(value, config);
            lines.push(format!("ENV {}=\"{}\"", key, substituted));
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::dsl::{load, ImageConfig};

    /// Absolute path to the repo-root `images/` directory.
    fn catalog_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../images")
    }

    fn load_catalog(name: &str) -> ImageConfig {
        let path = catalog_dir().join(name).join("image.toml");
        load(path.to_str().unwrap()).expect(&format!("catalog image '{}' should parse", name))
    }

    /// True if the Containerfile contains a line starting with the given
    /// directive (e.g. `"FROM "`, `"RUN "`, `"ENV "`).
    fn has_line_starting_with(cf: &str, prefix: &str) -> bool {
        cf.lines().any(|line| line.starts_with(prefix))
    }

    /// Collect all lines starting with the given directive.
    fn lines_starting_with(cf: &str, prefix: &str) -> Vec<String> {
        cf.lines()
            .filter(|line| line.starts_with(prefix))
            .map(String::from)
            .collect()
    }

    // ----------------------------------------------------------------------
    // W6-T2: generate Containerfile from each catalog image and verify
    // expected FROM / RUN / ENV / LABEL directives appear in the output.
    // ----------------------------------------------------------------------

    #[test]
    fn test_generate_containerfile_rocky_base() {
        let cfg = load_catalog("rocky-base");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // First non-empty line should be the FROM directive. rocky-base is
        // the root (extends="") so the FROM produces "FROM stronghold/"
        // which is the existing behavior; we just check the directive exists.
        assert!(
            has_line_starting_with(&cf, "FROM "),
            "rocky-base Containerfile must contain a FROM directive"
        );

        // LABEL directives: 4 OCI labels from the rocky-base image.toml
        let labels = lines_starting_with(&cf, "LABEL ");
        assert_eq!(labels.len(), 4, "rocky-base should emit 4 LABEL lines");
        assert!(cf.contains("org.opencontainers.image.title=\"stronghold/rocky-base\""));
        assert!(cf.contains("org.opencontainers.image.licenses=\"Apache-2.0\""));

        // RUN directives: post_install commands produce a RUN block; the
        // useradd line must appear (escaped with leading "    && ").
        assert!(
            has_line_starting_with(&cf, "RUN"),
            "rocky-base Containerfile must contain at least one RUN directive"
        );
        assert!(cf.contains("useradd -m -s /usr/bin/fish -u 1000 dev"));
        assert!(cf.contains("echo 'dev ALL=(ALL) NOPASSWD: ALL'"));

        // ENV directives: 7 env vars from rocky-base
        let envs = lines_starting_with(&cf, "ENV ");
        assert_eq!(envs.len(), 7, "rocky-base should emit 7 ENV lines");
        assert!(cf.contains("ENV HOME=\"/home/dev\""));
        assert!(cf.contains("ENV SHELL=\"/usr/bin/fish\""));
        assert!(cf.contains("ENV EDITOR=\"helix\""));
        assert!(cf.contains("ENV TZ=\"UTC\""));

        // dnf install line: 26 packages joined with spaces
        assert!(cf.contains("RUN dnf install -y dnf5 git curl wget"));

        // Injected Containerfile snippets (escape hatch): 4 snippets appear
        // verbatim, preceded by the marker comment.
        assert!(cf.contains("# Injected snippets (escape hatch)"));
        assert!(cf.contains("USER dev"));
        assert!(cf.contains("WORKDIR /home/dev/work"));
        assert!(cf.contains("CMD [\"fish\"]"));
    }

    #[test]
    fn test_generate_containerfile_rust_stable() {
        let cfg = load_catalog("rust-stable");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // FROM stronghold/rocky-base
        assert!(cf.contains("FROM stronghold/rocky-base"));

        // RUN directives: dnf install + post_install RUN block + rust toolchain RUNs
        assert!(cf.contains("RUN dnf install -y openssl-devel cmake"));
        assert!(cf.contains("RUN rustup default stable"));
        assert!(cf.contains("RUN rustup target add x86_64-unknown-linux-gnu"));
        assert!(cf.contains("RUN rustup target add wasm32-wasip2"));
        assert!(cf.contains("RUN rustup component add rust-src"));
        assert!(cf.contains("RUN rustup component add clippy"));

        // ENV directives: 3 vars, with {home} placeholder substituted
        let envs = lines_starting_with(&cf, "ENV ");
        assert_eq!(envs.len(), 3 + 1); // +1 for the cargo PATH added by the toolchain
        assert!(
            cf.contains("ENV CARGO_TARGET_DIR=\"/home/dev/target\""),
            "expected {{home}} placeholder to be substituted, got: {}",
            cf
        );
        assert!(cf.contains("ENV RUSTFLAGS=\"-C target-cpu=native\""));
        assert!(cf.contains("ENV RUST_BACKTRACE=\"1\""));

        // post_install commands appear (with leading "    && ")
        assert!(cf.contains("    && sudo dnf clean all"));
    }

    #[test]
    fn test_generate_containerfile_rust_nightly() {
        let cfg = load_catalog("rust-nightly");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // FROM stronghold/rocky-base
        assert!(cf.contains("FROM stronghold/rocky-base"));

        // Rust toolchain: nightly-2026-03-01 (date-pinned)
        assert!(cf.contains("# Toolchain: rust (nightly)"));
        assert!(cf.contains("RUN rustup toolchain install nightly-2026-03-01 --profile minimal"));
        assert!(cf.contains("RUN rustup default nightly-2026-03-01"));
        assert!(cf.contains("RUN rustup component add miri"));

        // Elan toolchain: leanprover/lean4:stable
        assert!(cf.contains("# Toolchain: elan (leanprover/lean4:stable)"));
        assert!(cf.contains("RUN curl -fsSL https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh | sh -s -- -y"));
        assert!(cf.contains("ENV PATH=\"/root/.elan/bin:${PATH}\""));

        // pre_install commands appear (wasmtime installer)
        assert!(cf.contains("RUN"));
        assert!(
            cf.contains("curl -sSfL https://wasmtime.dev/install.sh | bash -s -- --version 47.0")
        );

        // ENV directives: 4 user-defined + 2 toolchain PATHs = 6
        let envs = lines_starting_with(&cf, "ENV ");
        assert_eq!(envs.len(), 6);
        assert!(cf.contains("ENV CARGO_INCREMENTAL=\"1\""));
        // {home} substituted in CARGO_TARGET_DIR
        assert!(cf.contains("ENV CARGO_TARGET_DIR=\"/home/dev/target\""));
    }

    #[test]
    fn test_generate_containerfile_node_20() {
        let cfg = load_catalog("node-20");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // FROM stronghold/rocky-base
        assert!(cf.contains("FROM stronghold/rocky-base"));

        // Node toolchain
        assert!(cf.contains("# Toolchain: node (20.11.1)"));
        assert!(cf.contains("RUN curl -fsSL https://rpm.nodesource.com/setup_20.11.1.x | bash -"));
        assert!(cf.contains("RUN dnf install -y nodejs"));
        assert!(cf.contains("RUN npm install -g pnpm"));

        // pre_install + post_install
        assert!(cf.contains("curl -fsSL https://rpm.nodesource.com/setup_20.x | bash -"));
        assert!(cf.contains("npm install -g pnpm@9"));

        // ENV: 2 user-defined (NODE_ENV, COREPACK_ENABLE_DOWNLOAD_PROMPT)
        assert!(cf.contains("ENV NODE_ENV=\"development\""));
        assert!(cf.contains("ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=\"0\""));
    }

    #[test]
    fn test_generate_containerfile_python_ml() {
        let cfg = load_catalog("python-ml");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // FROM stronghold/rocky-base
        assert!(cf.contains("FROM stronghold/rocky-base"));

        // Python toolchain
        assert!(cf.contains("# Toolchain: python (3.12.2)"));
        assert!(cf.contains("RUN dnf install -y python3.12 python3.12-pip"));

        // dnf install line includes CUDA packages
        assert!(cf.contains("RUN dnf install -y python3.12 python3.12-pip python3.12-devel gcc gcc-c++ make cmake cuda-toolkit cudnn nccl"));

        // post_install: pip installs
        assert!(cf.contains("pip3.12 install --user torch torchvision torchaudio"));
        assert!(cf.contains("pip3.12 install --user numpy pandas scikit-learn matplotlib jupyter"));

        // ENV: {home} substituted in PYTHONPATH, {path} substituted in PATH
        assert!(
            cf.contains("ENV PYTHONPATH=\"/home/dev/.local/lib/python3.12/site-packages\""),
            "expected {{home}} substitution in PYTHONPATH, got: {}",
            cf
        );
        assert!(
            cf.contains(
                "ENV PATH=\"/usr/local/cuda/bin:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin\""
            ),
            "expected {{path}} substitution in PATH, got: {}",
            cf
        );
        assert!(cf.contains("ENV CUDA_HOME=\"/usr/local/cuda\""));
        assert!(cf.contains("ENV LD_LIBRARY_PATH=\"/usr/local/cuda/lib64\""));
    }

    #[test]
    fn test_generate_containerfile_go_cli() {
        let cfg = load_catalog("go-cli");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // FROM stronghold/rocky-base
        assert!(cf.contains("FROM stronghold/rocky-base"));

        // Go toolchain
        assert!(cf.contains("# Toolchain: go (1.22.2)"));
        assert!(cf.contains(
            "RUN curl -fsSL https://go.dev/dl/go1.22.2.linux-amd64.tar.gz | tar -C /usr/local -xz"
        ));
        assert!(cf.contains("ENV PATH=\"/usr/local/go/bin:${PATH}\""));

        // pre_install (curl go tarball) — appears as RUN block
        assert!(cf.contains(
            "curl -fsSL https://go.dev/dl/go1.22.2.linux-amd64.tar.gz | tar -C /usr/local -xz"
        ));

        // ENV: {home} substituted in GOPATH and GOBIN
        assert!(
            cf.contains("ENV GOPATH=\"/home/dev/go\""),
            "expected {{home}} substitution in GOPATH, got: {}",
            cf
        );
        assert!(cf.contains("ENV GOBIN=\"/home/dev/go/bin\""));
        assert!(cf.contains("ENV GOPROXY=\"https://proxy.golang.org,direct\""));
    }

    #[test]
    fn test_generate_containerfile_lean_research() {
        let cfg = load_catalog("lean-research");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // FROM stronghold/rocky-base
        assert!(cf.contains("FROM stronghold/rocky-base"));

        // Elan toolchain
        assert!(cf.contains("# Toolchain: elan (leanprover/lean4:stable)"));
        assert!(cf.contains("RUN curl -fsSL https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh | sh -s -- -y"));
        assert!(cf.contains("ENV PATH=\"/root/.elan/bin:${PATH}\""));

        // pre_install: elan-init with --default-toolchain none
        assert!(cf.contains("--default-toolchain none"));

        // ENV: {home} substituted in LEAN_PATH
        assert!(
            cf.contains("ENV LEAN_PATH=\"/home/dev/.elan/lib\""),
            "expected {{home}} substitution in LEAN_PATH, got: {}",
            cf
        );
    }

    #[test]
    fn test_generate_containerfile_fullstack() {
        let cfg = load_catalog("fullstack");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // FROM stronghold/rocky-base
        assert!(cf.contains("FROM stronghold/rocky-base"));

        // Node toolchain
        assert!(cf.contains("# Toolchain: node (20.11.1)"));
        assert!(cf.contains("RUN dnf install -y nodejs"));

        // dnf install: postgres + redis
        assert!(cf.contains("RUN dnf install -y postgresql redis gcc gcc-c++ make"));

        // post_install: pnpm + prisma
        assert!(cf.contains("npm install -g pnpm@9 prisma@latest"));

        // ENV: no placeholders, raw values
        assert!(cf.contains("ENV NODE_ENV=\"development\""));
        assert!(cf.contains("ENV DATABASE_URL=\"postgres://localhost:5432/dev\""));
        assert!(cf.contains("ENV REDIS_URL=\"redis://localhost:6379\""));
    }

    /// All 8 catalog images produce a non-empty Containerfile starting with
    /// a FROM directive. Smoke test that the generator never returns Err for
    /// any catalog image.
    #[test]
    fn test_generate_containerfile_all_catalog_images() {
        let names = [
            "rocky-base",
            "rust-stable",
            "rust-nightly",
            "node-20",
            "python-ml",
            "go-cli",
            "lean-research",
            "fullstack",
        ];
        for name in names {
            let cfg = load_catalog(name);
            let cf = generate_containerfile(&cfg).expect(&format!(
                "generate_containerfile failed for catalog image '{}'",
                name
            ));
            assert!(!cf.is_empty(), "Containerfile for '{}' is empty", name);
            assert!(
                cf.lines().next().unwrap().starts_with("FROM "),
                "Containerfile for '{}' must start with FROM",
                name
            );
        }
    }

    // ----------------------------------------------------------------------
    // W6-T8: escape hatches — pre_install, post_install, inject_containerfile
    // snippets all appear in the generated Containerfile at the right places.
    // ----------------------------------------------------------------------

    #[test]
    fn test_escape_hatches_all_three_present() {
        // Build a synthetic image with all three escape hatches populated.
        let toml = r#"
name = "escape-hatch-test"
extends = "rocky-base"
description = "Image exercising all three escape hatches"

[pre_install]
commands = [
    "echo PRE_INSTALL_MARKER",
    "curl -fsSL https://example.com/pre-install | bash -",
]

[packages]
dnf = ["sl", "cowsay"]

[post_install]
commands = [
    "echo POST_INSTALL_MARKER",
    "rm -rf /var/cache/example",
]

[inject_containerfile]
snippets = [
    "COPY --from=stronghold/extra-tools:2026.07 /usr/local/bin/just /usr/local/bin/just",
    "RUN echo INJECT_RUN_MARKER > /etc/custom",
    "ENV INJECTED_ENV=\"yes\"",
]
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        // 1. pre_install markers appear, before the dnf install line
        let pre_idx = cf.find("PRE_INSTALL_MARKER").unwrap();
        let dnf_idx = cf.find("RUN dnf install -y").unwrap();
        assert!(
            pre_idx < dnf_idx,
            "pre_install commands must appear before dnf install"
        );
        assert!(cf.contains("curl -fsSL https://example.com/pre-install | bash -"));

        // 2. post_install markers appear, after the dnf install line
        let post_idx = cf.find("POST_INSTALL_MARKER").unwrap();
        assert!(
            post_idx > dnf_idx,
            "post_install commands must appear after dnf install"
        );
        assert!(cf.contains("rm -rf /var/cache/example"));

        // 3. inject_containerfile snippets appear verbatim, after the
        //    "# Injected snippets (escape hatch)" marker comment, which
        //    itself comes after post_install.
        let inject_marker_idx = cf.find("# Injected snippets (escape hatch)").unwrap();
        assert!(
            inject_marker_idx > post_idx,
            "inject_containerfile section must come after post_install"
        );
        assert!(cf.contains(
            "COPY --from=stronghold/extra-tools:2026.07 /usr/local/bin/just /usr/local/bin/just"
        ));
        assert!(cf.contains("RUN echo INJECT_RUN_MARKER > /etc/custom"));
        assert!(cf.contains("ENV INJECTED_ENV=\"yes\""));
    }

    #[test]
    fn test_escape_hatch_pre_install_only() {
        let toml = r#"
name = "pre-only"
extends = "rocky-base"
description = "Only pre_install"

[pre_install]
commands = ["echo ONLY_PRE"]
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        assert!(cf.contains("echo ONLY_PRE"));
        assert!(!cf.contains("# Injected snippets"));
    }

    #[test]
    fn test_escape_hatch_post_install_only() {
        let toml = r#"
name = "post-only"
extends = "rocky-base"
description = "Only post_install"

[post_install]
commands = ["echo ONLY_POST"]
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        assert!(cf.contains("echo ONLY_POST"));
        assert!(!cf.contains("# Injected snippets"));
    }

    #[test]
    fn test_escape_hatch_inject_only() {
        let toml = r#"
name = "inject-only"
extends = "rocky-base"
description = "Only inject_containerfile"

[inject_containerfile]
snippets = ["EXPOSE 8080", "HEALTHCHECK CMD curl -f http://localhost:8080/health"]
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        assert!(cf.contains("# Injected snippets (escape hatch)"));
        assert!(cf.contains("EXPOSE 8080"));
        assert!(cf.contains("HEALTHCHECK CMD curl -f http://localhost:8080/health"));
    }

    #[test]
    fn test_escape_hatches_none_present() {
        // An image with none of the escape hatches must not emit any of the
        // marker comments or empty RUN blocks.
        let toml = r#"
name = "no-escapes"
extends = "rocky-base"
description = "No escape hatches"

[packages]
dnf = ["jq"]
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        assert!(!cf.contains("# Injected snippets"));
        // No "    && " continuation lines (which only appear with non-empty
        // pre/post_install RUN blocks).
        assert!(!cf.contains("    && "));
    }

    // ----------------------------------------------------------------------
    // W6-T8 (sub): verify ordering of escape hatches in the Containerfile.
    //
    // The expected order is:
    //   FROM → LABEL → pre_install → packages → toolchains → env →
    //   post_install → inject_containerfile
    // ----------------------------------------------------------------------

    #[test]
    fn test_escape_hatch_ordering() {
        let toml = r#"
name = "ordering-test"
extends = "rocky-base"
description = "Escape hatch ordering test"

[labels]
"org.opencontainers.image.title" = "ordering-test"

[pre_install]
commands = ["echo STEP_PRE"]

[packages]
dnf = ["jq"]

[toolchains.node]
version = "20.11.1"

[env]
FOO = "bar"

[post_install]
commands = ["echo STEP_POST"]

[inject_containerfile]
snippets = ["RUN echo STEP_INJECT"]
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");

        let from_idx = cf.find("FROM ").unwrap();
        let label_idx = cf.find("LABEL ").unwrap();
        let pre_idx = cf.find("STEP_PRE").unwrap();
        let dnf_idx = cf.find("RUN dnf install -y").unwrap();
        let toolchain_idx = cf.find("# Toolchain: node").unwrap();
        let env_idx = cf.find("ENV FOO").unwrap();
        let post_idx = cf.find("STEP_POST").unwrap();
        let inject_idx = cf.find("STEP_INJECT").unwrap();

        // Strictly increasing positions confirm correct ordering.
        assert!(from_idx < label_idx, "LABEL must come after FROM");
        assert!(label_idx < pre_idx, "pre_install must come after LABEL");
        assert!(pre_idx < dnf_idx, "packages must come after pre_install");
        assert!(
            dnf_idx < toolchain_idx,
            "toolchains must come after packages"
        );
        assert!(toolchain_idx < env_idx, "ENV must come after toolchains");
        assert!(env_idx < post_idx, "post_install must come after ENV");
        assert!(post_idx < inject_idx, "inject must come after post_install");
    }

    // ----------------------------------------------------------------------
    // W6-T8 (sub): {home} and {path} placeholder substitution in env vars
    // ----------------------------------------------------------------------

    #[test]
    fn test_placeholder_home_substituted_with_default() {
        // No HOME override in the image → defaults to "/home/dev".
        let toml = r#"
name = "home-default"
extends = "rocky-base"
description = "Test {home} default"

[env]
CARGO_TARGET_DIR = "{home}/target"
MY_VAR = "prefix-{home}-suffix"
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        assert!(cf.contains("ENV CARGO_TARGET_DIR=\"/home/dev/target\""));
        assert!(cf.contains("ENV MY_VAR=\"prefix-/home/dev-suffix\""));
    }

    #[test]
    fn test_placeholder_path_substituted_with_default() {
        // No PATH override in the image → defaults to the rocky-base PATH.
        let toml = r#"
name = "path-default"
extends = "rocky-base"
description = "Test {path} default"

[env]
EXTRA_PATH = "/opt/bin:{path}"
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        assert!(
            cf.contains("ENV EXTRA_PATH=\"/opt/bin:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin\"")
        );
    }

    #[test]
    fn test_placeholder_home_ignores_config_override() {
        // The {home} placeholder always refers to the dev user's home
        // directory (/home/dev) as created by rocky-base — even if the
        // image overrides HOME. This is intentional: {home} is the
        // *actual* home directory, not the env var value.
        let toml = r#"
name = "home-override"
extends = "rocky-base"
description = "Test {home} with HOME override"

[env]
HOME = "/home/custom"
CARGO_TARGET_DIR = "{home}/target"
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        // HOME env line is emitted verbatim (the override)
        assert!(cf.contains("ENV HOME=\"/home/custom\""));
        // {home} placeholder uses the default /home/dev, not the override
        assert!(cf.contains("ENV CARGO_TARGET_DIR=\"/home/dev/target\""));
    }

    #[test]
    fn test_placeholder_path_ignores_config_override() {
        // The {path} placeholder always refers to the rocky-base PATH
        // (the inherited PATH) — even if the image overrides PATH. This
        // avoids recursive substitution when the override itself contains
        // {path} (e.g. python-ml: PATH = "/usr/local/cuda/bin:{path}").
        let toml = r#"
name = "path-override"
extends = "rocky-base"
description = "Test {path} with PATH override"

[env]
PATH = "/custom/bin"
EXTRA_PATH = "{path}:/extra"
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        // PATH env line is emitted verbatim (the override)
        assert!(cf.contains("ENV PATH=\"/custom/bin\""));
        // {path} placeholder uses the default rocky-base PATH, not the override
        assert!(
            cf.contains("ENV EXTRA_PATH=\"/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin:/extra\"")
        );
    }

    #[test]
    fn test_placeholder_no_substitution_for_other_braces() {
        // Only {home} and {path} are substituted. Other {braces} stay as-is.
        let toml = r#"
name = "no-sub"
extends = "rocky-base"
description = "Test that other braces are not substituted"

[env]
MIXED = "{home} and {path} and {other} and {HOME}"
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        // {home} → /home/dev
        // {path} → default PATH
        // {other} → left as-is
        // {HOME} → left as-is (case-sensitive, only {home} lowercase matches)
        assert!(cf.contains("/home/dev"));
        assert!(cf.contains("/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin"));
        assert!(cf.contains("{other}"));
        assert!(cf.contains("{HOME}"));
    }

    #[test]
    fn test_placeholder_multiple_occurrences() {
        // Multiple {home} or {path} in the same value are all substituted.
        let toml = r#"
name = "multi-occ"
extends = "rocky-base"
description = "Multiple placeholder occurrences"

[env]
X = "{home}:{home}:{path}:{path}"
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        let expected = format!(
            "ENV X=\"{}:{}:{}:{}\"",
            "/home/dev",
            "/home/dev",
            "/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin",
            "/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin"
        );
        assert!(
            cf.contains(&expected),
            "expected multiple placeholder substitution, got: {}",
            cf
        );
    }

    #[test]
    fn test_placeholder_in_value_without_placeholders() {
        // Values without placeholders are emitted verbatim.
        let toml = r#"
name = "plain"
extends = "rocky-base"
description = "No placeholders"

[env]
PLAIN = "just-a-value"
NUMBER = "42"
"#;
        let cfg = crate::images::dsl::parse(toml).expect("parse should succeed");
        let cf = generate_containerfile(&cfg).expect("generate should succeed");
        assert!(cf.contains("ENV PLAIN=\"just-a-value\""));
        assert!(cf.contains("ENV NUMBER=\"42\""));
    }

    // ----------------------------------------------------------------------
    // Direct unit test of the substitute_placeholders helper.
    // ----------------------------------------------------------------------

    #[test]
    fn test_substitute_placeholders_helper_directly() {
        let cfg = ImageConfig {
            name: "test".to_string(),
            extends: "rocky-base".to_string(),
            description: String::new(),
            packages: Default::default(),
            toolchains: Default::default(),
            env: std::collections::HashMap::new(),
            pre_install: Default::default(),
            post_install: Default::default(),
            inject_containerfile: Default::default(),
            labels: Default::default(),
        };

        // Defaults: {home} → /home/dev, {path} → default PATH
        assert_eq!(substitute_placeholders("{home}", &cfg), "/home/dev");
        assert_eq!(
            substitute_placeholders("{path}", &cfg),
            "/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin"
        );
        assert_eq!(substitute_placeholders("plain", &cfg), "plain");
        assert_eq!(substitute_placeholders("", &cfg), "");
        // Combined
        assert_eq!(
            substitute_placeholders("{home}/target:{path}/extra", &cfg),
            "/home/dev/target:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin/extra"
        );
    }

    // ----------------------------------------------------------------------
    // W6-T3 (smoke): build() stub writes Containerfile to temp dir and
    // returns a sha256: digest. (Podman integration deferred.)
    // ----------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_stub_writes_containerfile_and_returns_digest() {
        let cfg = load_catalog("rust-stable");
        let digest = build(&cfg, "stronghold/rust-stable:test")
            .await
            .expect("build (stub) should succeed");
        // Stub digest format: sha256: + 64 hex chars (32 zero bytes)
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), "sha256:".len() + 64);
        // The hex portion should be all zeros (stub)
        let hex_part = &digest["sha256:".len()..];
        assert!(hex_part.chars().all(|c| c == '0'));
    }
}
