# ADR 0009: Rocky Linux as the universal base image

## Status

Accepted

## Context

Stronghold needs a base OS for all container images. Every image in the catalog `extends` from this base. The choice affects:
- ABI compatibility (what binaries can run)
- Package manager (how to install software)
- Support lifecycle (how long the base is maintained)
- Size (smaller is better for containers)
- Familiarity (what developers know)

## Decision

Use **Rocky Linux 9** as the universal base image for all Stronghold images.

## Alternatives Considered

### Alpine Linux
- **Pros:** Tiny (~5MB), fast to pull
- **Cons:** Uses musl libc instead of glibc — breaks many Rust static builds, Python wheels, and pre-compiled binaries. Package ecosystem (apk) is smaller. Not representative of production environments.

### Ubuntu
- **Pros:** Familiar, large package ecosystem
- **Cons:** Deviates from how RHEL-world production actually works. Different package manager (apt vs dnf). Different service management. If agents are building stuff that ships to production, matching the prod OS removes a class of "works in dev, breaks in prod" surprises.

### Debian
- **Pros:** Stable, large ecosystem
- **Cons:** Same as Ubuntu — not RHEL-compatible. Older packages.

### Fedora
- **Pros:** Cutting-edge, RHEL-compatible
- **Cons:** Too cutting-edge — packages change frequently, not suitable for a stable base. 13-month support lifecycle.

### RHEL Universal Base Image (UBI)
- **Pros:** Official Red Hat, RHEL-compatible
- **Cons:** Requires Red Hat subscription for some packages. Not fully open-source.

### Rocky Linux 9
- **Pros:**
  - RHEL-compatible ABI (binaries built on Rocky run on RHEL and vice versa)
  - 10-year support lifecycle
  - dnf5 package manager (fast, parallel downloads)
  - Fully open-source, no subscription
  - If agents build stuff that ships to production, matching prod OS eliminates "works in dev, breaks in prod"
  - Larger than Alpine (~250MB) but still reasonable
- **Cons:**
  - Larger than Alpine
  - Less familiar to developers who only know Debian/Ubuntu

## Consequences

### Positive
- Production-faithful: if your prod runs RHEL/Rocky/CentOS, dev matches prod
- 10-year support lifecycle — no frequent base image churn
- dnf5 is fast and parallel
- Glibc — no musl compatibility issues
- Fully open-source
- Well-documented, large community

### Negative
- Larger image size than Alpine (~250MB vs ~5MB)
  - Mitigated by layer caching — all images share the rocky-base layer
- Less familiar to Debian/Ubuntu users
  - Mitigated by the DSL — users write `image.toml`, not raw Containerfiles

### Neutral
- Rocky Linux is a community rebuild of RHEL, maintained by the Rocky Enterprise Software Foundation
- Rocky 9 is the current stable version (EOL: May 2032)

## Implementation

### rocky-base image

```toml
# images/rocky-base/image.toml
name = "rocky-base"
extends = ""
description = "Rocky Linux 9 minimal base with essential dev tools"

[packages]
dnf = [
    "dnf5", "git", "curl", "wget", "openssh-clients", "rsync",
    "jq", "httpie", "vim", "helix", "tmux", "htop", "btop",
    "fish", "bash", "sudo", "procps-ng", "findutils",
    "ripgrep", "fd-find", "tree", "less", "man-db",
    "ca-certificates", "tzdata",
]

[env]
LANG = "en_US.UTF-8"
TZ = "UTC"
EDITOR = "helix"
SHELL = "/usr/bin/fish"

[post_install]
commands = [
    "sudo dnf clean all",
    "rm -rf /var/cache/dnf",
    "useradd -m -s /usr/bin/fish -u 1000 dev",
    "echo 'dev ALL=(ALL) NOPASSWD: ALL' >> /etc/sudoers.d/dev",
]
```

### All other images extend from rocky-base

```toml
# images/rust-nightly/image.toml
name = "rust-nightly"
extends = "rocky-base"  # ← every image extends from rocky-base
```

## References

- [Rocky Linux](https://rockylinux.org/)
- [Rocky Linux 9 documentation](https://docs.rockylinux.org/)
- [dnf5](https://github.com/rpm-software-management/dnf5)
