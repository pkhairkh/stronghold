#!/usr/bin/env python3
"""Build a Stronghold image from its image.toml spec.

Handles the full image stack:
  rocky-base (root) → rust-stable (extends rocky-base) → rust-nightly (extends rocky-base)
  python-ml, fullstack, node-20, go-cli, lean-research all extend rocky-base

Usage:
  python3 build_image.py <image-name> [--tag latest] [--push] [--no-cache]

Examples:
  python3 build_image.py rocky-base --push
  python3 build_image.py rust-stable --push
  python3 build_image.py rust-nightly --push

The script:
  1. Reads images/<name>/image.toml
  2. Resolves `extends` to determine the parent image:
     - "" or absent → use rockylinux:9-minimal as the actual base
     - "rocky-base" → use localhost:30500/stronghold/rocky-base:<tag>
     - "rust-stable" → use localhost:30500/stronghold/rust-stable:<tag>
  3. Generates a Containerfile from the spec
  4. Builds with `buildah bud`
  5. Tags as localhost:30500/stronghold/<name>:<tag>
  6. Pushes to the local Stronghold registry (if --push)
  7. Imports into k3s containerd image store (if --push)

For Rocky 9 base (extends=""), uses microdnf + EPEL + CRB.
For derived images (extends a stronghold image), uses dnf (installed by rocky-base).
"""
import argparse
import os
import subprocess
import sys
import tomllib
from pathlib import Path

REGISTRY = os.environ.get("STRONGHOLD_REGISTRY", "localhost:30500")
REPO_ROOT = Path(os.environ.get("STRONGHOLD_REPO", "/root/stronghold"))
IMAGES_DIR = REPO_ROOT / "images"

# Packages that aren't in Rocky 9 base repos — skip them on the root image.
# (Derived images that extend rocky-base have dnf + EPEL + CRB already set up.)
SKIP_ON_ROCKY9 = {
    "dnf5",       # Rocky 10+ only
    "helix",      # not in EPEL 9
    "btop",       # not in EPEL 9
    "httpie",     # deps not in EPEL 9 (python3.9dist(pygments) issue)
}

# Packages that ARE in Rocky 9 base repos (no EPEL/CRB needed)
BASE_PACKAGES = {
    "gcc", "gcc-c++", "make", "cmake", "pkgconf-pkg-config", "binutils",
    "kernel-headers", "glibc-devel", "libtool", "autoconf", "automake",
    "openssl-devel", "sqlite-devel", "zlib-devel", "libffi-devel",
    "libxml2-devel", "libxslt-devel",
    "git", "openssh-clients", "openssh-server",
    "vim", "vim-default-editor", "nano",
    "fish", "bash", "bash-completion",
    "tree", "less", "man-db", "tmux", "htop", "procps-ng", "findutils",
    "ca-certificates", "tzdata", "rsync", "wget", "curl", "sudo",
    "python3", "python3-pip",
    "iproute", "iputils", "bind-utils", "nmap-ncat", "tcpdump",
    "glibc-langpack-en",
}


def build_containerfile(name: str, spec: dict, parent_ref: str) -> str:
    """Generate a Containerfile from the image.toml spec."""
    extends = spec.get("extends", "")
    is_root = extends == "" or extends is None

    lines = [f"# Auto-generated from images/{name}/image.toml", f"FROM {parent_ref}", ""]

    # Derived images inherit USER dev from rocky-base. Switch back to root
    # for the install steps; inject_containerfile snippets switch back to
    # dev at the end.
    if not is_root:
        lines.append("USER root")
        lines.append("")

    # Labels
    labels = spec.get("labels", {})
    if labels:
        for k, v in labels.items():
            lines.append(f'LABEL {k}="{v}"')
        lines.append("")

    # Pre-install scripts (run as root, before packages)
    pre_install = spec.get("pre_install", {}).get("commands", [])
    if pre_install:
        lines.append("RUN " + " && \\\n    ".join(pre_install))
        lines.append("")

    # Packages
    dnf = spec.get("packages", {}).get("dnf", [])
    if dnf:
        if is_root:
            # Root image: rockylinux:9-minimal ships microdnf only. Install
            # dnf + dnf-plugins-core first, then enable EPEL + CRB, then
            # install the full package list with dnf.
            #
            # Also drop packages that don't exist in Rocky 9 repos.
            SKIP = SKIP_ON_ROCKY9 | {"vim-default-editor"}
            available = [p for p in dnf if p not in SKIP]
            skipped = [p for p in dnf if p in SKIP]
            if skipped:
                print(f"  (skipping on rocky-base: {', '.join(skipped)})")
            pkgs = " ".join(available)
            lines.append(
                "RUN microdnf -y install dnf dnf-plugins-core epel-release && "
                "dnf config-manager --set-enabled crb && "
                f"dnf -y install {pkgs} && "
                "dnf clean all"
            )
        else:
            # Derived image: rocky-base already has dnf + EPEL + CRB
            pkgs = " ".join(dnf)
            lines.append(f"RUN dnf -y install {pkgs} && dnf clean all")
        lines.append("")

    # Toolchains (rust, elan, etc.)
    toolchains = spec.get("toolchains", {})
    for toolchain_name, tc_spec in toolchains.items():
        if toolchain_name == "rust":
            channel = tc_spec.get("channel", "stable")
            date = tc_spec.get("date")
            targets = tc_spec.get("targets", [])
            components = tc_spec.get("components", [])

            # Install rustup as the dev user (so cargo goes to /home/dev/.cargo)
            # We're currently root (set above for derived images); switch to dev.
            rustup_cmd = "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
            if date:
                toolchain_spec = f"{channel}-{date}"
            else:
                toolchain_spec = channel
            rustup_cmd += f" --default-toolchain {toolchain_spec} --profile minimal"

            # Add components
            if components:
                comp_str = " ".join(f"-c {c}" for c in components)
                rustup_cmd += f" {comp_str}"

            # Run as dev user (su - dev -s /bin/bash -c) so $HOME is /home/dev.
            # Force bash because dev's default shell is fish, which has a
            # different PATH resolution and breaks npm/cargo/pip lookups.
            lines.append(f'RUN su - dev -s /bin/bash -c "{rustup_cmd}"')
            lines.append('ENV PATH="/home/dev/.cargo/bin:${PATH}"')

            # Add targets (also as dev)
            if targets:
                for target in targets:
                    lines.append(f'RUN su - dev -s /bin/bash -c "/home/dev/.cargo/bin/rustup target add {target}"')
            lines.append("")

        elif toolchain_name == "elan":
            # Lean 4 toolchain — install as dev user so it goes to /home/dev/.elan
            channel = tc_spec.get("channel", "leanprover/lean4:stable")
            date = tc_spec.get("date")
            toolchain_spec = channel if not date else f"{channel}-{date}"
            elan_cmd = (
                "curl https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh -sSf | "
                f"sh -s -- -y --default-toolchain {toolchain_spec}"
            )
            lines.append(f'RUN su - dev -s /bin/bash -c "{elan_cmd}"')
            lines.append('ENV PATH="/home/dev/.elan/bin:${PATH}"')
            lines.append("")

        elif toolchain_name == "node":
            # Node.js — installed via pre_install (NodeSource rpm) or direct download.
            # The toolchain entry just sets the env; the actual install is in pre_install.
            version = tc_spec.get("version", "20")
            lines.append(f"# Node.js {version} (installed via pre_install NodeSource repo)")
            lines.append("")

        elif toolchain_name == "go":
            # Go — installed via pre_install (direct tarball download).
            version = tc_spec.get("version", "1.22")
            lines.append(f"# Go {version} (installed via pre_install tarball download)")
            lines.append('ENV PATH="/usr/local/go/bin:${PATH}"')
            lines.append("")

        elif toolchain_name == "python":
            # Python — installed via dnf packages (python3.12 etc.) in the packages section.
            version = tc_spec.get("version", "3.12")
            lines.append(f"# Python {version} (installed via dnf packages above)")
            lines.append("")

    # env vars
    env = spec.get("env", {})
    if env:
        for k, v in env.items():
            # Substitute placeholders
            v = v.replace("{home}", "/home/dev")
            v = v.replace("{path}", "/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin:/home/dev/.cargo/bin")
            # Quote values that contain spaces (ENV RUSTFLAGS="-C target-cpu=native")
            # otherwise Docker splits on the space and only sets the first word.
            if " " in v:
                lines.append(f'ENV {k}="{v}"')
            else:
                lines.append(f"ENV {k}={v}")
        lines.append("")

    # post_install commands
    post = spec.get("post_install", {}).get("commands", [])
    if post:
        if is_root:
            # Root image: copy the SDK into the image first, then run post_install.
            # The SDK file is in the build context (copied by main()).
            lines.append("COPY stronghold-agent.sh /tmp/stronghold-agent.sh")
            lines.append("RUN " + " && \\\n    ".join(post))
        else:
            # Derived image: run as dev user (su - dev -c) so pip --user
            # installs to /home/dev/.local, npm -g installs work via sudo, etc.
            # Strip leading `sudo ` since dev has NOPASSWD sudo.
            adapted = []
            for cmd in post:
                # pip --user installs go to the user's home → run as dev
                if "--user" in cmd:
                    cmd_clean = cmd.replace("sudo ", "")
                    adapted.append(f'su - dev -s /bin/bash -c "{cmd_clean}"')
                # npm install -g / pnpm install -g need root (writes to /usr/lib/node_modules)
                # → run as root, strip sudo (we're already root)
                elif "npm install -g" in cmd or "pnpm install -g" in cmd:
                    adapted.append(cmd.replace("sudo ", ""))
                else:
                    # Run as root (dnf clean all, rm -rf, etc.)
                    adapted.append(cmd.replace("sudo ", ""))
            lines.append("RUN " + " && \\\n    ".join(adapted))
        lines.append("")

    # inject_containerfile snippets (USER, WORKDIR, VOLUME, CMD)
    snippets = spec.get("inject_containerfile", {}).get("snippets", [])
    if snippets:
        for s in snippets:
            lines.append(s)
    elif not is_root:
        # Derived images without inject_containerfile: switch back to dev
        # (inherited from rocky-base) + keep WORKDIR.
        lines.append("USER dev")
        lines.append('WORKDIR /home/dev/work')
    lines.append("")

    return "\n".join(lines)


def resolve_parent_ref(extends: str, tag: str) -> str:
    """Resolve the parent image reference from the `extends` field."""
    if extends == "" or extends is None:
        # Root image — use rockylinux:9-minimal
        return "rockylinux:9-minimal"
    else:
        # Derived image — use the Stronghold registry
        return f"{REGISTRY}/stronghold/{extends}:{tag}"


def main():
    parser = argparse.ArgumentParser(description="Build a Stronghold image from its image.toml spec")
    parser.add_argument("name", help="Image name (e.g. rocky-base, rust-stable, rust-nightly)")
    parser.add_argument("--tag", default="latest", help="Image tag (default: latest)")
    parser.add_argument("--push", action="store_true", help="Push to the Stronghold registry after build")
    parser.add_argument("--no-cache", action="store_true", help="Pass --no-cache to buildah")
    parser.add_argument("--dry-run", action="store_true", help="Print Containerfile without building")
    args = parser.parse_args()

    spec_path = IMAGES_DIR / args.name / "image.toml"
    if not spec_path.exists():
        print(f"ERROR: image spec not found: {spec_path}", file=sys.stderr)
        return 1

    print(f"📖 reading {spec_path}", flush=True)
    with spec_path.open("rb") as f:
        spec = tomllib.load(f)

    extends = spec.get("extends", "")
    parent_ref = resolve_parent_ref(extends, args.tag)
    print(f"🔗 parent: {parent_ref}", flush=True)

    containerfile = build_containerfile(args.name, spec, parent_ref)
    print("=== generated Containerfile ===")
    print(containerfile)
    print("=== end Containerfile ===\n", flush=True)

    if args.dry_run:
        return 0

    # Write Containerfile to a build context dir
    ctx = Path(f"/tmp/stronghold-{args.name}-build")
    ctx.mkdir(parents=True, exist_ok=True)
    (ctx / "Containerfile").write_text(containerfile)

    # Copy the Stronghold agent SDK into the build context so rocky-base
    # can install it at /usr/local/bin/stronghold-agent.sh
    sdk_src = REPO_ROOT / "agent" / "stronghold-agent.sh"
    if sdk_src.exists():
        import shutil
        shutil.copy2(sdk_src, ctx / "stronghold-agent.sh")
        print(f"📦 copied stronghold-agent.sh into build context", flush=True)
    else:
        print(f"⚠️  stronghold-agent.sh not found at {sdk_src}", file=sys.stderr)

    # Build with buildah
    local_ref = f"stronghold/{args.name}:{args.tag}"
    print(f"🔨 buildah bud -t {local_ref} {ctx}", flush=True)
    cmd = ["buildah", "bud", "-f", "Containerfile", "-t", local_ref, str(ctx)]
    if args.no_cache:
        cmd.insert(2, "--no-cache")
    r = subprocess.run(cmd, check=False)
    if r.returncode != 0:
        print(f"❌ buildah bud failed (rc={r.returncode})", file=sys.stderr)
        return r.returncode

    if not args.push:
        print(f"\n✅ built {local_ref} (local only — pass --push to push to registry)")
        return 0

    # Tag for the registry
    registry_ref = f"{REGISTRY}/stronghold/{args.name}:{args.tag}"
    print(f"🏷️  buildah tag {local_ref} {registry_ref}", flush=True)
    r = subprocess.run(["buildah", "tag", local_ref, registry_ref], check=False)
    if r.returncode != 0:
        return r.returncode

    # Push to the local registry (HTTP, insecure)
    print(f"📤 buildah push --tls-verify=false {registry_ref}", flush=True)
    r = subprocess.run(
        ["buildah", "push", "--tls-verify=false", registry_ref],
        check=False,
    )
    if r.returncode != 0:
        print(f"❌ buildah push failed (rc={r.returncode})", file=sys.stderr)
        return r.returncode

    # Verify push
    print(f"\n=== verifying push ===", flush=True)
    r = subprocess.run(
        ["curl", "-s", f"http://{REGISTRY}/v2/stronghold/{args.name}/tags/list"],
        check=False,
        capture_output=True,
        text=True,
    )
    print(r.stdout)

    # Pull into k3s containerd so future pods can use it without a remote pull
    print(f"=== pulling into k3s containerd ===", flush=True)
    r = subprocess.run(["crictl", "pull", registry_ref], check=False)
    if r.returncode != 0:
        print(f"⚠️  crictl pull failed (rc={r.returncode}) — image is in registry, k3s will pull on first pod schedule")

    print(f"\n✅ {args.name}:{args.tag} built + pushed to {registry_ref}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
