#!/usr/bin/env python3
"""Convert images/rocky-base/image.toml into a Containerfile and build it
with buildah. Then tag it as localhost:30500/stronghold/rocky-base:latest
and push to the local Stronghold registry.

This is the bootstrap path — once the gateway's image-builder routes are
implemented (next phase), agents will invoke
`POST /admin/images/build { name: "rocky-base" }` instead of running
this script directly.
"""
import os
import subprocess
import sys
import tomllib  # py 3.11+
from pathlib import Path

REGISTRY = os.environ.get("STRONGHOLD_REGISTRY", "localhost:30500")
IMAGE_NAME = "stronghold/rocky-base"
TAG = "latest"

def main():
    spec_path = Path("/root/stronghold/images/rocky-base/image.toml")
    print(f"reading {spec_path}", flush=True)
    with spec_path.open("rb") as f:
        spec = tomllib.load(f)

    # The rocky-base extends "" (no parent) — use rockylinux:9-minimal as the
    # actual base image.
    extends = spec.get("extends", "") or "rockylinux:9-minimal"
    if "/" not in extends and not extends.startswith("localhost"):
        # Treat short names without a slash as external base images.
        base = extends
    else:
        base = f"stronghold/{extends}"

    lines = [f"# Auto-generated from {spec_path}", f"FROM {base}", ""]

    # Labels
    labels = spec.get("labels", {})
    if labels:
        for k, v in labels.items():
            lines.append(f'LABEL {k}="{v}"')
        lines.append("")

    # dnf packages
    dnf = spec.get("packages", {}).get("dnf", [])
    if dnf:
        # rockylinux:9-minimal ships microdnf (not dnf). Use microdnf for
        # the base image; derived images (which extend rocky-base and have
        # dnf installed) use dnf directly.
        #
        # Some packages in the spec are not in the Rocky 9 base repos
        # (dnf5 is Rocky 10+, helix needs EPEL). For the bootstrap build
        # we drop them — agents that need them can install via dnf inside
        # a running pod, or via a derived image that adds EPEL.
        SKIP_ON_9 = {"dnf5", "helix", "btop", "httpie"}
        if extends == "rockylinux:9-minimal" or extends == "":
            dnf = [p for p in dnf if p not in SKIP_ON_9]
            # Enable EPEL + CRB (CodeReady Builder) so we can install ripgrep,
            # fd-find, fish, etc. that aren't in the base Rocky 9 repos.
            epel_setup = (
                "microdnf -y install epel-release && "
                "/usr/bin/crb enable && "
            )
            pkgs = " ".join(dnf)
            lines.append(
                f"RUN {epel_setup}microdnf -y install {pkgs} && microdnf clean all"
            )
        else:
            pkgs = " ".join(dnf)
            lines.append(f"RUN dnf -y install {pkgs} && dnf clean all")
        lines.append("")

    # env vars
    env = spec.get("env", {})
    if env:
        for k, v in env.items():
            lines.append(f"ENV {k}={v}")
        lines.append("")

    # post_install commands
    post = spec.get("post_install", {}).get("commands", [])
    if post:
        # Adapt the spec's `sudo dnf clean all` to `microdnf clean all` for
        # the base image (dnf isn't installed yet — microdnf is).
        adapted = []
        for cmd in post:
            if extends == "rockylinux:9-minimal" or extends == "":
                cmd = cmd.replace("dnf clean all", "microdnf clean all")
                cmd = cmd.replace("sudo dnf clean all", "microdnf clean all")
            adapted.append(cmd)
        # Add git safe.directory exception so the dev user can operate on
        # git repos in /home/dev/work even when the PVC mount is owned by
        # root (k8s fsGroup only chowns contents, not the mount point itself).
        # Use printf for the tab character (echo -e isn't portable).
        adapted.append(
            "mkdir -p /home/dev/.config/git && "
            "printf '[safe]\\n\\tdirectory = *\\n' > /home/dev/.config/git/config && "
            "chown -R dev:dev /home/dev/.config"
        )
        lines.append("RUN " + " && \\\n    ".join(adapted))
        lines.append("")

    # inject_containerfile snippets
    snippets = spec.get("inject_containerfile", {}).get("snippets", [])
    for s in snippets:
        lines.append(s)
    lines.append("")

    containerfile = "\n".join(lines)
    print("=== generated Containerfile ===")
    print(containerfile)
    print("=== end Containerfile ===\n", flush=True)

    # Write Containerfile to a temp dir + build context
    ctx = Path("/tmp/stronghold-rocky-base-build")
    ctx.mkdir(parents=True, exist_ok=True)
    (ctx / "Containerfile").write_text(containerfile)

    # Build with buildah — uses host containerd storage (no daemon needed)
    local_ref = f"{IMAGE_NAME}:{TAG}"
    print(f"buildah bud -t {local_ref} {ctx}", flush=True)
    r = subprocess.run(
        ["buildah", "bud", "-f", "Containerfile", "-t", local_ref, str(ctx)],
        check=False,
    )
    if r.returncode != 0:
        print(f"buildah bud failed (rc={r.returncode})", file=sys.stderr)
        return r.returncode

    # Tag for the registry
    registry_ref = f"{REGISTRY}/{IMAGE_NAME}:{TAG}"
    print(f"buildah tag {local_ref} {registry_ref}", flush=True)
    r = subprocess.run(["buildah", "tag", local_ref, registry_ref], check=False)
    if r.returncode != 0:
        return r.returncode

    # Push to the local registry (HTTP, insecure)
    print(f"buildah push --tls-verify=false {registry_ref}", flush=True)
    r = subprocess.run(
        ["buildah", "push", "--tls-verify=false", registry_ref],
        check=False,
    )
    if r.returncode != 0:
        print(f"buildah push failed (rc={r.returncode})", file=sys.stderr)
        return r.returncode

    # Verify
    print(f"\n=== verifying push ===", flush=True)
    r = subprocess.run(
        ["curl", "-s", f"http://{REGISTRY}/v2/{IMAGE_NAME}/tags/list"],
        check=False,
        capture_output=True,
        text=True,
    )
    print(r.stdout)

    # Also push to k3s containerd image store so future pods can pull it
    # without re-fetching from the registry.
    print(f"\n=== importing into k3s containerd ===", flush=True)
    r = subprocess.run(
        ["buildah", "push", "--tls-verify=false",
         registry_ref,
         f"containers-storage:{registry_ref}"],
        check=False,
    )
    # If containers-storage push fails (different storage driver), try ctr
    if r.returncode != 0:
        print("(containers-storage push skipped — using registry pull instead)")

    print(f"\n✅ rocky-base built + pushed to {registry_ref}", flush=True)
    return 0

if __name__ == "__main__":
    sys.exit(main())
