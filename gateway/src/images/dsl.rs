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

    #[serde(default, deserialize_with = "deserialize_toolchains")]
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

/// Custom deserializer for the `toolchains` map.
///
/// The `Toolchain` enum uses `#[serde(untagged)]`, which means serde tries
/// each variant in declaration order. Since `Node`, `Python`, and `Go`
/// all have the same shape (`{ version: String }`), serde would always
/// pick `Node` for any of them — losing the type information.
///
/// This deserializer uses the *map key* (e.g. `"go"` in `[toolchains.go]`)
/// to pick the correct variant. Unknown toolchain names produce a clear
/// error rather than silently mis-tagging as `Node`.
fn deserialize_toolchains<'de, D>(
    deserializer: D,
) -> std::result::Result<std::collections::HashMap<String, Toolchain>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{MapAccess, Visitor};
    use std::fmt;

    struct ToolchainsVisitor;

    impl<'de> Visitor<'de> for ToolchainsVisitor {
        type Value = std::collections::HashMap<String, Toolchain>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a map of toolchain name to toolchain config")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut out: std::collections::HashMap<String, Toolchain> =
                std::collections::HashMap::new();
            while let Some(key) = map.next_key::<String>()? {
                let value: toml::Value = map.next_value()?;
                let toolchain = match key.as_str() {
                    "rust" => {
                        let channel = value
                            .get("channel")
                            .and_then(toml::Value::as_str)
                            .ok_or_else(|| {
                                serde::de::Error::custom(
                                    "rust toolchain requires a `channel` string field",
                                )
                            })?
                            .to_string();
                        let date = value
                            .get("date")
                            .and_then(toml::Value::as_str)
                            .map(String::from);
                        let targets = value
                            .get("targets")
                            .and_then(toml::Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(toml::Value::as_str)
                                    .map(String::from)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let components = value
                            .get("components")
                            .and_then(toml::Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(toml::Value::as_str)
                                    .map(String::from)
                                    .collect()
                            })
                            .unwrap_or_default();
                        Toolchain::Rust {
                            channel,
                            date,
                            targets,
                            components,
                        }
                    }
                    "node" => {
                        let version = value
                            .get("version")
                            .and_then(toml::Value::as_str)
                            .ok_or_else(|| {
                                serde::de::Error::custom(
                                    "node toolchain requires a `version` string field",
                                )
                            })?
                            .to_string();
                        Toolchain::Node { version }
                    }
                    "python" => {
                        let version = value
                            .get("version")
                            .and_then(toml::Value::as_str)
                            .ok_or_else(|| {
                                serde::de::Error::custom(
                                    "python toolchain requires a `version` string field",
                                )
                            })?
                            .to_string();
                        Toolchain::Python { version }
                    }
                    "go" => {
                        let version = value
                            .get("version")
                            .and_then(toml::Value::as_str)
                            .ok_or_else(|| {
                                serde::de::Error::custom(
                                    "go toolchain requires a `version` string field",
                                )
                            })?
                            .to_string();
                        Toolchain::Go { version }
                    }
                    "elan" => {
                        let channel = value
                            .get("channel")
                            .and_then(toml::Value::as_str)
                            .ok_or_else(|| {
                                serde::de::Error::custom(
                                    "elan toolchain requires a `channel` string field",
                                )
                            })?
                            .to_string();
                        let date = value
                            .get("date")
                            .and_then(toml::Value::as_str)
                            .map(String::from);
                        Toolchain::Elan { channel, date }
                    }
                    // Unknown toolchain name — return a clear error. The 5
                    // known toolchain kinds (rust, node, python, go, elan)
                    // cover the entire v1 catalog. New kinds must be added
                    // here explicitly.
                    _ => {
                        return Err(serde::de::Error::custom(format!(
                            "unknown toolchain '{}' (expected one of: rust, node, python, go, elan)",
                            key
                        )))
                    }
                };
                out.insert(key, toolchain);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_map(ToolchainsVisitor)
}

/// Parse an `image.toml` file.
pub fn parse(content: &str) -> Result<ImageConfig> {
    let config: ImageConfig = toml::from_str(content)?;

    // Validate: must extend from rocky-base (directly or transitively).
    //
    // Allowed `extends` values:
    //   - ""               — only valid for the root image (`name == "rocky-base"`)
    //   - "rocky-base"     — direct child of the universal root
    //   - "stronghold/X"   — transitively derived from a Stronghold image whose
    //                        own chain eventually resolves to `rocky-base`
    //
    // Anything else is rejected with a clear error message.
    let is_root = config.name == "rocky-base" && config.extends.is_empty();
    let is_direct_child = config.extends == "rocky-base";
    let is_transitive = config.extends.starts_with("stronghold/");
    if !is_root && !is_direct_child && !is_transitive {
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Absolute path to the repo-root `images/` directory.
    ///
    /// Tests resolve catalog `image.toml` files from here so they work
    /// regardless of the working directory `cargo test` is invoked from.
    /// `CARGO_MANIFEST_DIR` is the gateway package directory
    /// (`<repo>/gateway/`), so `../images` reaches the catalog at
    /// `<repo>/images/`.
    fn catalog_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../images")
    }

    fn load_catalog(name: &str) -> ImageConfig {
        let path = catalog_dir().join(name).join("image.toml");
        load(path.to_str().unwrap()).expect(&format!("catalog image '{}' should parse", name))
    }

    // ----------------------------------------------------------------------
    // W6-T1: parse each of the 8 catalog images, verify all fields
    // ----------------------------------------------------------------------

    #[test]
    fn test_parse_rocky_base() {
        let cfg = load_catalog("rocky-base");
        assert_eq!(cfg.name, "rocky-base");
        assert_eq!(cfg.extends, "");
        assert_eq!(
            cfg.description,
            "Rocky Linux 9 minimal base with essential dev tools"
        );
        // Packages: 25 dnf entries, empty apt
        assert_eq!(cfg.packages.dnf.len(), 25);
        assert!(cfg.packages.apt.is_empty());
        assert!(cfg.packages.dnf.contains(&"fish".to_string()));
        assert!(cfg.packages.dnf.contains(&"sudo".to_string()));
        assert!(cfg.packages.dnf.contains(&"helix".to_string()));
        // No toolchains on the base image
        assert!(cfg.toolchains.is_empty());
        // 7 env vars
        assert_eq!(cfg.env.len(), 7);
        assert_eq!(cfg.env.get("HOME").unwrap(), "/home/dev");
        assert_eq!(cfg.env.get("SHELL").unwrap(), "/usr/bin/fish");
        assert_eq!(cfg.env.get("EDITOR").unwrap(), "helix");
        assert_eq!(cfg.env.get("TZ").unwrap(), "UTC");
        // No pre_install
        assert!(cfg.pre_install.commands.is_empty());
        // 6 post_install commands
        assert_eq!(cfg.post_install.commands.len(), 6);
        assert!(cfg
            .post_install
            .commands
            .iter()
            .any(|c| c.contains("useradd -m -s /usr/bin/fish -u 1000 dev")));
        // 4 injected Containerfile snippets (USER, WORKDIR, VOLUME, CMD)
        assert_eq!(cfg.inject_containerfile.snippets.len(), 4);
        assert!(cfg
            .inject_containerfile
            .snippets
            .iter()
            .any(|s| s.starts_with("USER ")));
        assert!(cfg
            .inject_containerfile
            .snippets
            .iter()
            .any(|s| s.starts_with("WORKDIR ")));
        assert!(cfg
            .inject_containerfile
            .snippets
            .iter()
            .any(|s| s.starts_with("CMD ")));
        // 4 OCI labels
        assert_eq!(cfg.labels.len(), 4);
        assert_eq!(
            cfg.labels.get("org.opencontainers.image.title").unwrap(),
            "stronghold/rocky-base"
        );
        assert_eq!(
            cfg.labels.get("org.opencontainers.image.licenses").unwrap(),
            "Apache-2.0"
        );
    }

    #[test]
    fn test_parse_rust_stable() {
        let cfg = load_catalog("rust-stable");
        assert_eq!(cfg.name, "rust-stable");
        assert_eq!(cfg.extends, "rocky-base");
        assert_eq!(cfg.description, "Rocky 9 + Rust stable + common targets");
        // 6 dnf packages
        assert_eq!(cfg.packages.dnf.len(), 6);
        assert!(cfg.packages.dnf.contains(&"openssl-devel".to_string()));
        assert!(cfg.packages.dnf.contains(&"cmake".to_string()));
        // 1 toolchain: rust stable
        assert_eq!(cfg.toolchains.len(), 1);
        match cfg.toolchains.get("rust").unwrap() {
            Toolchain::Rust {
                channel,
                date,
                targets,
                components,
            } => {
                assert_eq!(channel, "stable");
                assert!(date.is_none());
                assert_eq!(targets.len(), 3);
                assert!(targets.contains(&"wasm32-wasip2".to_string()));
                assert_eq!(components.len(), 3);
                assert!(components.contains(&"clippy".to_string()));
            }
            other => panic!("expected Rust toolchain, got {:?}", other),
        }
        // 3 env vars
        assert_eq!(cfg.env.len(), 3);
        assert_eq!(cfg.env.get("CARGO_TARGET_DIR").unwrap(), "{home}/target");
        // 2 post_install commands
        assert_eq!(cfg.post_install.commands.len(), 2);
        // No inject_containerfile snippets
        assert!(cfg.inject_containerfile.snippets.is_empty());
        // 2 OCI labels
        assert_eq!(cfg.labels.len(), 2);
    }

    #[test]
    fn test_parse_rust_nightly() {
        let cfg = load_catalog("rust-nightly");
        assert_eq!(cfg.name, "rust-nightly");
        assert_eq!(cfg.extends, "rocky-base");
        assert!(cfg.description.contains("Rust nightly"));
        // 2 toolchains: rust (nightly, pinned to 2026-03-01) + elan
        assert_eq!(cfg.toolchains.len(), 2);
        match cfg.toolchains.get("rust").unwrap() {
            Toolchain::Rust {
                channel,
                date,
                targets,
                components,
            } => {
                assert_eq!(channel, "nightly");
                assert_eq!(date.as_deref(), Some("2026-03-01"));
                assert_eq!(targets.len(), 3);
                assert!(components.contains(&"miri".to_string()));
            }
            other => panic!("expected Rust toolchain, got {:?}", other),
        }
        match cfg.toolchains.get("elan").unwrap() {
            Toolchain::Elan { channel, date } => {
                assert_eq!(channel, "leanprover/lean4:stable");
                assert_eq!(date.as_deref(), Some("2026-02-15"));
            }
            other => panic!("expected Elan toolchain, got {:?}", other),
        }
        // 4 env vars
        assert_eq!(cfg.env.len(), 4);
        assert_eq!(cfg.env.get("CARGO_INCREMENTAL").unwrap(), "1");
        // 1 pre_install command (wasmtime installer)
        assert_eq!(cfg.pre_install.commands.len(), 1);
        assert!(cfg.pre_install.commands[0].contains("wasmtime.dev"));
        // 2 post_install commands
        assert_eq!(cfg.post_install.commands.len(), 2);
    }

    #[test]
    fn test_parse_node_20() {
        let cfg = load_catalog("node-20");
        assert_eq!(cfg.name, "node-20");
        assert_eq!(cfg.extends, "rocky-base");
        assert_eq!(cfg.packages.dnf.len(), 3);
        // 1 toolchain: node 20.11.1
        assert_eq!(cfg.toolchains.len(), 1);
        match cfg.toolchains.get("node").unwrap() {
            Toolchain::Node { version } => assert_eq!(version, "20.11.1"),
            other => panic!("expected Node toolchain, got {:?}", other),
        }
        // 2 env vars
        assert_eq!(cfg.env.len(), 2);
        assert_eq!(cfg.env.get("NODE_ENV").unwrap(), "development");
        // 1 pre_install, 3 post_install
        assert_eq!(cfg.pre_install.commands.len(), 1);
        assert_eq!(cfg.post_install.commands.len(), 3);
    }

    #[test]
    fn test_parse_python_ml() {
        let cfg = load_catalog("python-ml");
        assert_eq!(cfg.name, "python-ml");
        assert_eq!(cfg.extends, "rocky-base");
        assert_eq!(cfg.packages.dnf.len(), 10);
        assert!(cfg.packages.dnf.contains(&"cuda-toolkit".to_string()));
        assert!(cfg.packages.dnf.contains(&"cudnn".to_string()));
        // 1 toolchain: python 3.12.2
        assert_eq!(cfg.toolchains.len(), 1);
        match cfg.toolchains.get("python").unwrap() {
            Toolchain::Python { version } => assert_eq!(version, "3.12.2"),
            other => panic!("expected Python toolchain, got {:?}", other),
        }
        // 4 env vars, with {home} and {path} placeholders
        assert_eq!(cfg.env.len(), 4);
        assert_eq!(
            cfg.env.get("PYTHONPATH").unwrap(),
            "{home}/.local/lib/python3.12/site-packages"
        );
        assert_eq!(cfg.env.get("PATH").unwrap(), "/usr/local/cuda/bin:{path}");
        assert_eq!(cfg.env.get("CUDA_HOME").unwrap(), "/usr/local/cuda");
        // No pre_install, 3 post_install (pip installs)
        assert!(cfg.pre_install.commands.is_empty());
        assert_eq!(cfg.post_install.commands.len(), 3);
    }

    #[test]
    fn test_parse_go_cli() {
        let cfg = load_catalog("go-cli");
        assert_eq!(cfg.name, "go-cli");
        assert_eq!(cfg.extends, "rocky-base");
        assert_eq!(cfg.packages.dnf.len(), 2);
        // 1 toolchain: go 1.22.2
        assert_eq!(cfg.toolchains.len(), 1);
        match cfg.toolchains.get("go").unwrap() {
            Toolchain::Go { version } => assert_eq!(version, "1.22.2"),
            other => panic!("expected Go toolchain, got {:?}", other),
        }
        // 3 env vars with {home} placeholders
        assert_eq!(cfg.env.len(), 3);
        assert_eq!(cfg.env.get("GOPATH").unwrap(), "{home}/go");
        assert_eq!(cfg.env.get("GOBIN").unwrap(), "{home}/go/bin");
        assert_eq!(
            cfg.env.get("GOPROXY").unwrap(),
            "https://proxy.golang.org,direct"
        );
        // 1 pre_install (curl go tarball), 1 post_install (dnf clean)
        assert_eq!(cfg.pre_install.commands.len(), 1);
        assert_eq!(cfg.post_install.commands.len(), 1);
    }

    #[test]
    fn test_parse_lean_research() {
        let cfg = load_catalog("lean-research");
        assert_eq!(cfg.name, "lean-research");
        assert_eq!(cfg.extends, "rocky-base");
        assert_eq!(cfg.packages.dnf.len(), 5);
        // 1 toolchain: elan
        assert_eq!(cfg.toolchains.len(), 1);
        match cfg.toolchains.get("elan").unwrap() {
            Toolchain::Elan { channel, date } => {
                assert_eq!(channel, "leanprover/lean4:stable");
                assert_eq!(date.as_deref(), Some("2026-02-15"));
            }
            other => panic!("expected Elan toolchain, got {:?}", other),
        }
        // 1 env var
        assert_eq!(cfg.env.len(), 1);
        assert_eq!(cfg.env.get("LEAN_PATH").unwrap(), "{home}/.elan/lib");
        // 1 pre_install (elan installer), 2 post_install (clean up)
        assert_eq!(cfg.pre_install.commands.len(), 1);
        assert_eq!(cfg.post_install.commands.len(), 2);
    }

    #[test]
    fn test_parse_fullstack() {
        let cfg = load_catalog("fullstack");
        assert_eq!(cfg.name, "fullstack");
        assert_eq!(cfg.extends, "rocky-base");
        // 5 dnf packages including postgres + redis
        assert_eq!(cfg.packages.dnf.len(), 5);
        assert!(cfg.packages.dnf.contains(&"postgresql".to_string()));
        assert!(cfg.packages.dnf.contains(&"redis".to_string()));
        // 1 toolchain: node 20.11.1
        assert_eq!(cfg.toolchains.len(), 1);
        match cfg.toolchains.get("node").unwrap() {
            Toolchain::Node { version } => assert_eq!(version, "20.11.1"),
            other => panic!("expected Node toolchain, got {:?}", other),
        }
        // 3 env vars (no placeholders)
        assert_eq!(cfg.env.len(), 3);
        assert_eq!(
            cfg.env.get("DATABASE_URL").unwrap(),
            "postgres://localhost:5432/dev"
        );
        assert_eq!(cfg.env.get("REDIS_URL").unwrap(), "redis://localhost:6379");
        // 1 pre_install, 3 post_install (pnpm + prisma + clean)
        assert_eq!(cfg.pre_install.commands.len(), 1);
        assert_eq!(cfg.post_install.commands.len(), 3);
        // post_install includes pnpm and prisma global installs
        assert!(cfg
            .post_install
            .commands
            .iter()
            .any(|c| c.contains("pnpm@9")));
        assert!(cfg
            .post_install
            .commands
            .iter()
            .any(|c| c.contains("prisma@latest")));
    }

    /// All 8 catalog images parse successfully. This guards against future
    /// regressions where a catalog edit breaks the parser.
    #[test]
    fn test_parse_all_catalog_images_succeed() {
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
            assert_eq!(cfg.name, name, "name field mismatch for {}", name);
        }
    }

    // ----------------------------------------------------------------------
    // W6-T1: inheritance — all images extend rocky-base (directly or
    // transitively). For the v1 catalog every derived image extends
    // rocky-base directly, but the parser also accepts `stronghold/*` for
    // transitive inheritance (e.g. `extends = "stronghold/rust-stable"`).
    // ----------------------------------------------------------------------

    #[test]
    fn test_all_catalog_images_extend_rocky_base() {
        let catalog = [
            ("rocky-base", ""), // root — empty extends
            ("rust-stable", "rocky-base"),
            ("rust-nightly", "rocky-base"),
            ("node-20", "rocky-base"),
            ("python-ml", "rocky-base"),
            ("go-cli", "rocky-base"),
            ("lean-research", "rocky-base"),
            ("fullstack", "rocky-base"),
        ];

        for (name, expected_parent) in catalog {
            let cfg = load_catalog(name);
            assert_eq!(
                cfg.extends, expected_parent,
                "image '{}' has unexpected extends value",
                name
            );

            if name == "rocky-base" {
                // root: no parent, but name must be exactly "rocky-base"
                assert!(cfg.extends.is_empty());
            } else {
                // Direct children: extends == "rocky-base"
                // Transitive children: extends starts with "stronghold/"
                let is_direct = cfg.extends == "rocky-base";
                let is_transitive = cfg.extends.starts_with("stronghold/");
                assert!(
                    is_direct || is_transitive,
                    "image '{}' does not extend rocky-base directly or transitively (extends='{}')",
                    name,
                    cfg.extends
                );
            }
        }
    }

    /// Synthetic test: an image declaring `extends = "stronghold/rust-stable"`
    /// should parse — this is the transitive inheritance path.
    #[test]
    fn test_parser_accepts_transitive_stronghold_extends() {
        let toml = r#"
name = "custom-rust"
extends = "stronghold/rust-stable"
description = "Custom image extending a Stronghold image"
"#;
        let cfg = parse(toml).expect("transitive stronghold/* extends should parse");
        assert_eq!(cfg.name, "custom-rust");
        assert_eq!(cfg.extends, "stronghold/rust-stable");
    }

    // ----------------------------------------------------------------------
    // W6-T1: negative tests — clear errors for malformed input
    // ----------------------------------------------------------------------

    #[test]
    fn test_negative_missing_name() {
        // `name` is a required field (no serde default). Missing it must
        // produce an Err, not a panic.
        let toml = r#"
extends = "rocky-base"
description = "Image with no name"
"#;
        let result = parse(toml);
        assert!(result.is_err(), "missing name should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("name") || err.contains("missing"),
            "error should mention the missing `name` field, got: {}",
            err
        );
    }

    #[test]
    fn test_negative_missing_extends() {
        // `extends` is a required field. Missing it must produce an Err.
        // Note: this is the truly-missing case (field absent), distinct from
        // the rocky-base root case where `extends = ""` is explicitly set.
        let toml = r#"
name = "my-image"
description = "Image with no extends"
"#;
        let result = parse(toml);
        assert!(result.is_err(), "missing extends should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("extends") || err.contains("missing"),
            "error should mention the missing `extends` field, got: {}",
            err
        );
    }

    #[test]
    fn test_negative_empty_extends_for_non_root() {
        // A non-root image declaring `extends = ""` must be rejected.
        let toml = r#"
name = "my-image"
extends = ""
description = "Non-root image with empty extends"
"#;
        let result = parse(toml);
        assert!(result.is_err(), "empty extends for non-root should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("rocky-base") || err.contains("stronghold"),
            "error should mention rocky-base or stronghold/*, got: {}",
            err
        );
    }

    #[test]
    fn test_negative_invalid_extends() {
        // extends pointing at a non-Stronghold image must be rejected.
        let toml = r#"
name = "my-image"
extends = "ubuntu"
description = "Image extending a non-Stronghold image"
"#;
        let result = parse(toml);
        assert!(result.is_err(), "invalid extends should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ubuntu"), "error should name the bad value, got: {}", err);
    }

    #[test]
    fn test_negative_invalid_toml() {
        // Garbage input that isn't valid TOML at all.
        let bad_inputs = [
            "this is not toml at all {{{}}",
            "[unclosed section",
            "name = ",                              // value missing
            "name = \"ok\"\nextends = \"rocky-base\n", // unterminated string
            "= nope",
            "[[array]]\ninvalid",
        ];
        for bad in bad_inputs {
            let result = parse(bad);
            assert!(
                result.is_err(),
                "expected parse error for invalid TOML: {:?}",
                bad
            );
        }
    }

    #[test]
    fn test_negative_empty_input() {
        // Empty string has no required fields → must error.
        let result = parse("");
        assert!(result.is_err(), "empty input should be rejected");
    }

    #[test]
    fn test_negative_unicode_garbage() {
        // Weird-but-valid UTF-8 (emoji, mixed scripts) shouldn't crash the
        // parser. It must return an Err, not panic.
        let weird = "🤖💥🎯 name = = = ∀∂∃";
        let result = parse(weird);
        assert!(result.is_err(), "weird-but-valid input should be rejected");
    }

    // ----------------------------------------------------------------------
    // W6-T1: property test — fuzz parser with random TOML, never panic
    // ----------------------------------------------------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Feeding arbitrary bytes (interpreted as UTF-8 if possible) into
        /// `parse()` must never panic. It may return `Ok` or `Err`, but the
        /// contract is: parser is total — no input crashes it.
        #[test]
        fn proptest_parser_never_panics_bytes(input in proptest::prelude::any::<Vec<u8>>()) {
            // parse() takes &str. If the random bytes aren't valid UTF-8,
            // a real caller would error out before reaching parse(); we
            // replace invalid sequences with the replacement char so we
            // still exercise the parser with arbitrary content.
            let s = String::from_utf8_lossy(&input);
            let _ = parse(&s);
        }

        /// Same property but with arbitrary valid Unicode strings, which
        /// exercise the TOML parser more aggressively with realistic input.
        #[test]
        fn proptest_parser_never_panics_unicode(input in proptest::prelude::any::<String>()) {
            let _ = parse(&input);
        }

        /// Fuzz with TOML-shaped input: random key=value lines with simple
        /// ASCII identifiers and values. Still must never panic.
        #[test]
        fn proptest_parser_never_panics_toml_shaped(
            lines in proptest::collection::vec(
                (proptest::string::string_regex("[a-z_]{1,8}").unwrap(),
                 proptest::string::string_regex("[a-zA-Z0-9 ./_-]{0,32}").unwrap()),
                0..20
            )
        ) {
            let mut s = String::new();
            for (k, v) in lines {
                s.push_str(&format!("{} = \"{}\"\n", k, v));
            }
            let _ = parse(&s);
        }
    }
}
