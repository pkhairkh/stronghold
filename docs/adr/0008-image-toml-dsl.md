# ADR 0008: image.toml DSL for image catalog

## Status

Accepted

## Context

Stronghold needs a catalog of dev machine images (Rust nightly, Node 20, Python ML, etc.). All images must extend from a shared Rocky Linux base. The question is how to author these image definitions.

## Decision

Use a **TOML-based DSL** (`image.toml`) that generates Containerfiles at build time.

## Alternatives Considered

### Hand-write Containerfiles (Dockerfiles)
- **Pros:** Universal, anyone can read them, no special tooling
- **Cons:** Repetitive (every image starts with the same `FROM rocky-base` + dnf install + cleanup pattern), easy to forget cleanup steps, hard to enforce consistency, toolchain pinning is manual

### Nix flakes → OCI via nix2container
- **Pros:** Maximum reproducibility, bit-for-bit identical images forever, declarative
- **Cons:** Requires learning Nix (steep learning curve), Nix ecosystem is niche, debugging Nix builds is painful, smaller community

### Buildah scripts
- **Pros:** Scriptable, no Dockerfile needed
- **Cons:** Still imperative (shell scripts), same repetition issues as Containerfiles

### Packer templates
- **Pros:** Good for VM images, HCL-based
- **Cons:** Designed for VMs, not containers; overkill for OCI images

## Consequences

### Positive
- Fast to author (20-line TOML vs 50-line Containerfile)
- Consistent structure enforced by the DSL
- Toolchain pinning is first-class (exact version + date)
- Escape hatches available (`pre_install`, `post_install`, `inject_containerfile`)
- Generates standard Containerfiles (transparent, debuggable)
- TOML is Rust-native (serde + `toml` crate)

### Negative
- The DSL is a thing we now maintain (docs, examples, edge cases)
- Not as reproducible as Nix (apt/dnf packages can drift)
- Limited to what the DSL can express (escape hatch mitigates this)

### Neutral
- For 5-10 images maintained by a small team, the DSL is the right tradeoff
- If the catalog grows past 30+ images with multiple contributors, consider migrating to Nix
- Migration path exists: add a `flake = "./flake.nix"` field as an escape hatch later

## Implementation

### DSL structure

```toml
name = "rust-nightly"
extends = "rocky-base"
description = "Rocky 9 + Rust nightly"

[packages]
dnf = ["qemu-user-static", "libgmp-devel"]

[toolchains.rust]
channel = "nightly"
date = "2026-03-01"
targets = ["x86_64-unknown-linux-gnu"]
components = ["rust-src", "clippy"]

[env]
CARGO_TARGET_DIR = "{home}/target"

[pre_install]
commands = ["curl -sSfL https://wasmtime.dev/install.sh | bash -s -- --version 47.0"]

[post_install]
commands = ["sudo dnf clean all"]

[inject_containerfile]
snippets = ["COPY --from=stronghold/extra-tools:2026.07 /usr/local/bin/just /usr/local/bin/just"]
```

### Build pipeline

```bash
stronghold image build images/rust-nightly/image.toml --tag stronghold/rust-nightly:2026.07
```

1. Parse `image.toml`
2. Resolve `extends` chain back to `rocky-base`
3. Generate Containerfile
4. Run `podman build` or `docker build`
5. Push to `ghcr.io` (public) or tenant registry (private)

### Escape hatches

Three levels of flexibility:
1. **DSL fields** — covers 80% of use cases (packages, toolchains, env, pre/post hooks)
2. **`inject_containerfile`** — paste arbitrary Containerfile directives for the last 20%
3. **Custom images** — tenants can build private images outside the catalog

## References

- [TOML specification](https://toml.io/)
- [serde + toml](https://docs.rs/toml)
- [OCI image format](https://github.com/opencontainers/image-spec)
