# Stronghold Image DSL

## Overview

The Image DSL is a TOML-based format for defining OCI images. All images `extends` from `stronghold/rocky-base` — the universal root. The DSL generates Containerfiles at build time.

---

## File Structure

Each image is defined in a file named `image.toml` inside a directory under `images/`:

```
images/
├── rocky-base/
│   └── image.toml
├── rust-nightly/
│   └── image.toml
└── my-custom-image/
    └── image.toml
```

---

## Fields

### Required

| Field | Type | Description |
|---|---|---|
| `name` | string | Image name (e.g., `rust-nightly`) |
| `extends` | string | Parent image (must be `rocky-base` or another `stronghold/*` image) |
| `description` | string | Human-readable description |

### Optional

| Field | Type | Description |
|---|---|---|
| `packages` | table | Package installations (see below) |
| `toolchains` | table | Pinned toolchain installations (see below) |
| `env` | table | Environment variables |
| `pre_install` | table | Shell commands to run before package installation |
| `post_install` | table | Shell commands to run after everything else |
| `inject_containerfile` | table | Escape hatch — paste arbitrary Containerfile directives |
| `labels` | table | OCI image labels |

---

## Packages

```toml
[packages]
dnf = ["qemu-user-static", "libgmp-devel", "openssl-devel"]
apt = []  # alias for dnf on rocky; ignored
```

---

## Toolchains

### Rust

```toml
[toolchains.rust]
channel = "nightly"          # or "stable" or "beta"
date = "2026-03-01"          # exact pin (optional for stable)
targets = [
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "wasm32-wasip2",
]
components = ["rust-src", "rustfmt", "clippy", "miri"]
```

### Node

```toml
[toolchains.node]
version = "20.11.1"          # exact pin
```

### Python

```toml
[toolchains.python]
version = "3.12.2"           # exact pin
```

### Go

```toml
[toolchains.go]
version = "1.22.2"           # exact pin
```

### Lean (via elan)

```toml
[toolchains.elan]
channel = "leanprover/lean4:stable"
date = "2026-02-15"          # exact pin
```

---

## Environment Variables

```toml
[env]
CARGO_TARGET_DIR = "{home}/target"
RUSTFLAGS = "-C target-cpu=native"
RUST_BACKTRACE = "1"
```

The placeholder `{home}` is replaced with the `dev` user's home directory (`/home/dev`) at build time. The placeholder `{path}` is replaced with the current `$PATH`.

---

## Pre/Post Install Scripts

```toml
[pre_install]
commands = [
  "curl -sSfL https://wasmtime.dev/install.sh | bash -s -- --version 47.0"
]

[post_install]
commands = [
  "sudo dnf clean all",
  "rm -rf /var/cache/dnf"
]
```

These are shell commands run at the specified point in the build:
- `pre_install`: Before package installation
- `post_install`: After everything else (packages, toolchains, env vars)

---

## Escape Hatch

For anything the DSL can't express, paste arbitrary Containerfile directives:

```toml
[inject_containerfile]
snippets = [
  "COPY --from=stronghold/extra-tools:2026.07 /usr/local/bin/just /usr/local/bin/just",
  "RUN echo 'custom build step' > /etc/custom"
]
```

These are inserted verbatim into the generated Containerfile, after all other directives.

---

## Labels

```toml
[labels]
org.opencontainers.image.title = "stronghold/rust-nightly"
org.opencontainers.image.description = "Rust nightly + Lean 4"
org.opencontainers.image.licenses = "MIT"
org.opencontainers.image.source = "https://github.com/pkhairkh/stronghold"
```

---

## Complete Example

```toml
name = "rust-nightly"
extends = "rocky-base"
description = "Rocky 9 + Rust nightly + common cross-targets + Lean 4"

[packages]
dnf = ["qemu-user-static", "libgmp-devel", "openssl-devel"]

[toolchains.rust]
channel = "nightly"
date = "2026-03-01"
targets = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "wasm32-wasip2"]
components = ["rust-src", "rustfmt", "clippy", "miri"]

[toolchains.elan]
channel = "leanprover/lean4:stable"
date = "2026-02-15"

[env]
CARGO_TARGET_DIR = "{home}/target"
RUSTFLAGS = "-C target-cpu=native"
RUST_BACKTRACE = "1"

[pre_install]
commands = [
  "curl -sSfL https://wasmtime.dev/install.sh | bash -s -- --version 47.0"
]

[post_install]
commands = [
  "sudo dnf clean all",
  "rm -rf /var/cache/dnf"
]

[inject_containerfile]
snippets = [
  "COPY --from=stronghold/extra-tools:2026.07 /usr/local/bin/just /usr/local/bin/just"
]

[labels]
org.opencontainers.image.title = "stronghold/rust-nightly"
org.opencontainers.image.description = "Rust nightly + Lean 4 for systems programming"
```

---

## Building

```bash
# Build an image from image.toml
stronghold image build images/rust-nightly/image.toml --tag stronghold/rust-nightly:2026.07

# List available images
stronghold image list

# Push to registry
stronghold image push stronghold/rust-nightly:2026.07
```

---

## Contributing to the Catalog

1. Create a new directory under `images/<your-image-name>/`
2. Write an `image.toml` file
3. All images must `extends` from `rocky-base` (directly or transitively)
4. Test the build locally
5. Submit a PR to `github.com/pkhairkh/stronghold`
6. CI builds, scans, and pushes the image to `ghcr.io/pkhairkh/stronghold/<name>:YYYY.MM`
