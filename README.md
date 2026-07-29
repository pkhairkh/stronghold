# Stronghold

> A self-hosted gateway that lets any AI agent with `bash` + `curl` request, attach to, and work inside isolated containerd workspaces on a fleet of Vultr boxes — with phone-approved sessions, post-quantum cryptography end-to-end, and SEV-SNP confidential computing.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![Status: Alpha](https://img.shields.io/badge/status-alpha-orange.svg)](CHANGELOG.md)

---

> ⚠️ **WARNING: ALPHA QUALITY — DO NOT DEPLOY IN PRODUCTION** ⚠️
>
> Stronghold is **alpha-stage software**. Core crypto primitives, the audit
> log, the k3s scheduler, and the PTY data path work, but many advertised
> security features (TLS, WebAuthn signature verification, quorum,
> anomaly scanning, E2E push, per-tenant network policies, SEV-SNP on
> real hardware) are **NOT wired into the running gateway**. See
> [Known Limitations](#known-limitations) below for the full list. Do not
> expose this gateway to untrusted networks or store sensitive data behind
> it.

---

## What is Stronghold?

Stronghold is a **control plane** that sits between AI agents (GLM-5.2 on chat.z.ai, or any agent with bash) and a fleet of Vultr VPS workers running k3s. Agents request machines via a small HTTP protocol; humans approve via phone (WebAuthn Face ID / YubiKey / multi-credential, through the phone's native browser — **no custom app**); approved sessions get full PTY access to an isolated containerd workspace built from a **Rocky Linux base image**. Every byte of every session is logged with **dual post-quantum signatures** (Ed25519 + ML-DSA-65). The gateway binary optionally runs inside an **AMD SEV-SNP confidential VM**, with attestation verifiable by the tenant's phone before any approval.

### Design principles

1. **Agents are the tenants.** Projects are an emergent property of what an agent happens to be doing. The gateway has no concept of "project" — only agents, machines, and sessions.
2. **Full shell, not per-command approval.** One tap opens a TTL'd workspace. The agent has full PTY access. Destructive operations trigger quorum re-approval mid-session.
3. **Post-quantum everywhere crypto lives.** TLS 1.3 + X25519Kyber768 hybrid transport. Ed25519 + ML-DSA-65 dual-signed audit log. X25519 + ML-KEM-768 hybrid push encryption.
4. **SEV-SNP in v1, not deferred.** The gateway runs inside an AMD SEV-SNP confidential VM. Audit signing keys are sealed to the launch measurement. The phone verifies attestation before approving any session.
5. **No custom phone app.** Browser + ntfy only, forever. All UI uses the phone's native browser via ntfy deep-links.
6. **No external providers for content.** ntfy is self-hosted. APNs/FCM are wake-up triggers only (iOS), and even that's optional.
7. **Multi-tenant from day one.** Every database table has `tenant_id` as the first column. No global state. No cross-tenant leakage possible at the data layer.
8. **Rocky Linux base for all images.** Every image in the catalog `extends` from `stronghold/rocky-base`. Single source of truth.
9. **Session-based approval with quorum for destructive ops.** Normal commands run freely within the session. Only operations matching a destructive pattern trigger mid-session re-approval.
10. **Fail closed.** Every failure mode denies rather than allows. Never schedules a pod without explicit, fresh, signed approval.

---

## Architecture

```
                  chat.z.ai (or any agent host)
   Tenants + phones ──────┐
   (Face ID/YubiKey,      │
    native browser only — │
    no custom app)        │
        │                 ▼
        │          ┌──────────────────┐
        │          │  AI agents       │
        │          │  (parallel chats)│
        │          └────────┬─────────┘
        │                   │ HTTPS + agent token
        │                   │ (TLS 1.3 + X25519Kyber768Draft00)
        ▼                   ▼
   ┌──────────────────────────────────────────────────────────┐
   │  Vultr: AMD SEV-SNP Confidential VM (attested)         │
   │  ┌──────────────────────────────────────────────────┐   │
   │  │  Stronghold Control Plane (single Rust binary)   │   │
   │  │  --features sev-snp (compiled in, v1 default)    │   │
   │  │                                                  │   │
   │  │  Tenant registry · Agent protocol (ORDER/RESUME/ │   │
   │  │  RELEASE/EXTEND) · WebAuthn (multi-cred, quorum) │   │
   │  │  Session manager · Audit (dual-signed) · ntfy    │   │
   │  │  (PQC E2E) · Image DSL · k3s scheduler · SEV-SNP │   │
   │  └──────────────────────────────────────────────────┘   │
   └──────────────────────┬───────────────────────────────────┘
                          │ k3s API (per-worker, TLS 1.3 + PQ)
                          ▼
   ┌──────────────────────────────────────────────────────────┐
   │  Vultr Worker Fleet (N boxes, k3s workers)              │
   │  ┌────────────┐ ┌────────────┐ ┌────────────┐            │
   │  │ k3s worker │ │ k3s worker │ │ k3s worker │            │
   │  │  + ntfd    │ │  + ntfd    │ │  + ntfd    │            │
   │  │  + registry│ │  + registry│ │  + registry│            │
   │  └────────────┘ └────────────┘ └────────────┘            │
   │       │              │              │                     │
   │       ▼              ▼              ▼                     │
   │  containerd pods (per-agent workspaces)                  │
   │  each: rocky-base + image.toml-defined toolchain         │
   └──────────────────────────────────────────────────────────┘
```

---

## Quick Start

### Prerequisites

- A Vultr account with API access
- A SEV-SNP-capable Vultr High Frequency plan (for the control plane)
- A phone with Face ID / Touch ID / biometric (or a YubiKey)
- Rust 1.75+ (for building from source)

### Install the control plane

```bash
curl -sL https://github.com/pkhairkh/stronghold/releases/latest/download/bootstrap.sh | bash
```

This will:
1. Verify the Vultr box is SEV-SNP-capable
2. Install Rust, build the `stronghold` binary (release, `--features sev-snp` default)
3. Launch the gateway inside an SEV-SNP guest
4. Generate Ed25519 + ML-DSA-65 audit keypair (sealed to measurement)
5. Generate X25519 + ML-KEM-768 push keypair
6. Initialize SQLite databases
7. Install systemd units
8. Print your `SETUP_PASSWORD`, `GATEWAY_URL`, and `SEV_SNP_MEASUREMENT`

### Enroll your phone

1. Install the [ntfy app](https://ntfy.sh) on your phone (F-Droid / Play Store / TestFlight)
2. Add your Stronghold ntfy server URL
3. Open the `GATEWAY_URL` in your phone's native browser
4. Enter `SETUP_PASSWORD`
5. **Verify the `SEV_SNP_MEASUREMENT`** matches `docs/MEASUREMENTS/v1.0.txt`
6. Face ID → credential enrolled

### Mint an agent token

```bash
stronghold agent-token mint --tenant default --ttl 86400
# → AGENT_TOKEN=vuma_tok_...
```

### Let an agent request a machine

> ⚠️ The gateway currently serves **plain HTTP** (`main.rs::serve()` binds a
> raw TCP listener; the TLS config is computed and discarded). Use `http://`,
> not `https://`, against the gateway until TLS is wired into startup.

```bash
curl -X POST http://your-gateway:8443/agent/order \
  -H "Authorization: Bearer $AGENT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "image": "stronghold/rust-nightly:2026.07",
    "ttl_secs": 14400,
    "reason": "iterate on a Lean proof",
    "compute": { "cpu": 4, "memory_gb": 8 }
  }'
```

Your phone buzzes. You tap **Approve**. Face ID. The agent gets a PTY endpoint. Full shell.

---

## Documentation

| Document | Description |
|---|---|
| [Threat Model](docs/THREAT_MODEL.md) | Threats addressed, threats not addressed, and the cryptographic trust model |
| [Agent Protocol](docs/PROTOCOL.md) | The ORDER/RESUME/RELEASE/EXTEND wire format reference |
| [Image DSL](docs/IMAGE_DSL.md) | How to author `image.toml` files for the catalog |
| [Operations](docs/OPERATIONS.md) | Tenant onboarding, key rotation, credential revocation, backup/restore |
| [Deployment](docs/DEPLOYMENT.md) | Single-box, multi-box fleet, and community-hosted patterns |
| [SEV-SNP](docs/SEV_SNP.md) | TEE setup, attestation verification, measurement signing |
| [ADRs](docs/adr/) | Architecture Decision Records — why each design choice was made |

---

## Repository Structure

```
stronghold/
├── gateway/          # Control plane binary (Rust + axum)
├── cli/              # stronghold CLI (Rust + clap)
├── images/           # Community image catalog (image.toml files)
├── phone/            # Browser-only enrollment page (no app)
├── setup/            # Bootstrap scripts + systemd units
├── docs/             # Full documentation + ADRs
└── examples/         # Deployment examples
```

---

## Status

**This is ALPHA software.** Core systems (agent protocol HTTP handlers,
agent token mint/verify, k3s pod scheduling, PTY proxy data path, dual-signed
audit log writer, crypto primitives, SQLite + migrations, image DSL parser +
Containerfile generator, CLI subcommand structure) are implemented and tested.
However, several security-critical features advertised elsewhere in the docs
are **NOT wired into the running gateway**. See [Known Limitations](#known-limitations)
for the exhaustive list.

See [CHANGELOG.md](CHANGELOG.md) for version history and [CONTRIBUTING.md](CONTRIBUTING.md) for how to contribute.

---

## Known Limitations

The following advertised features are **not implemented** or **not wired into
the running gateway** as of v1.0.1 (commit 1928864). The codebase is ALPHA
quality — the items below are tracked as follow-up work, not closed gaps.

Legend: ❌ = not implemented · ⚠️ = code exists but is not wired in · ✅ = works

### Transport & Crypto

1. ⚠️ **TLS is NOT enabled.** `main.rs::serve()` binds a plain TCP listener
   and serves HTTP. The TLS config in `crypto/tls.rs` is computed and then
   discarded (`let _tls_config = ...`).
2. ⚠️ **WebAuthn signature verification is NOT implemented.** Only assertion
   metadata is checked (challenge / origin / UV flag / RP ID hash). The
   actual cryptographic signature is never validated, so anyone who can
   craft a syntactically valid assertion blob can approve any session.
3. ⚠️ **Push notifications are NOT E2E-encrypted in production.** Only the
   test-only `send_encrypted_notification_to()` encrypts payloads; all
   production push paths send plaintext.
4. ⚠️ **Self-signed cert generation exists but is NOT wired into server
   startup.** `rcgen`-based `generate_self_signed_cert()` is implemented and
   unit-tested, but `serve()` never loads or uses it.
5. ⚠️ **SEV-SNP is untested on real hardware.** The `sev` crate is wired in
   with real ioctl calls, but the dev environment lacks `/dev/sev-guest`.
   The measurement registry (`docs/MEASUREMENTS/v1.0.txt`) is a placeholder.

### PTY proxy & session control

6. ❌ **Quorum for destructive ops is NOT enforced.** Data structures exist
   in `sessions/scopes.rs` but nothing calls them. The PTY proxy does not
   scan commands or block on re-approval.
7. ❌ **Anomaly scanning is NOT wired in.** `anomaly/mod.rs` has a working
   scanner but the PTY proxy never instantiates or calls it.
8. ❌ **Audit streaming to the PTY WebSocket is NOT implemented.**
   `routes/pty.rs::audit_stream()` immediately sends "not yet implemented"
   and returns.
9. ❌ **PTY WebSocket does NOT verify `connect_token`.** Anyone with the WS
   URL can attach to any session.
10. ⚠️ **Phone SSE pending-approvals stream is heartbeat-only.**
    `sessions/manager.rs::pending_approval_stream()` only emits heartbeats
    every 30 seconds. The phone never receives approval requests via SSE.
11. ⚠️ **Fail-closed is partial.** The PTY proxy fails open (does not block
    on missing quorum/anomaly). Other paths (ORDER without approval, missing
    agent token) fail closed.

### Fleet & build pipeline

12. ❌ **VPS escalation is a stub.** Returns `"stub-vps-id"` and `"0.0.0.0"`.
13. ❌ **`worker add` and `worker list` are stubs.** `add()` does nothing;
    `list()` returns an empty `Vec`.
14. ❌ **Image build never invokes podman/docker.** `images/builder.rs`
    parses the DSL and generates a Containerfile, but the actual build call
    is TODO.
15. ❌ **Image push / image pull are stubs.**

### Observability & multi-tenancy isolation

16. ❌ **Prometheus metrics endpoint does NOT exist.** There is no
    `/metrics` route. (A Grafana dashboard JSON is shipped but has nothing
    to scrape.)
17. ❌ **Per-tenant Kubernetes namespaces are NOT created.** All pods land
    in the `default` namespace; `tenant_id` is only a pod label.
18. ❌ **Per-tenant NetworkPolicy objects are NOT created.**

### CLI & tooling

- ❌ **`audit verify` only checks the hash chain.** Signature verification
  is a TODO. `OPERATIONS.md` shows the desired end-state output, but only
  `Hash chain: OK` is actually produced today.
- ❌ **`--dev` flag does not actually bypass the SEV-SNP check.** It sets a
  struct field, not the `STRONGHOLD_DEV` env var that `serve()` consults.
  Use `STRONGHOLD_DEV=1` instead.

### What DOES work (alpha scope)

- ✅ Agent protocol HTTP handlers (`ORDER`/`RESUME`/`RELEASE`/`EXTEND`)
- ✅ Agent token minting / verification
- ✅ k3s pod scheduling (real `kube-rs` API calls)
- ✅ PTY proxy data path (real `kube exec` via WebSocket, bidirectional byte
  pumping) — but see the connect_token gap above
- ✅ Audit log writer (real dual-signed Ed25519 + ML-DSA-65, hash-chained)
- ✅ Crypto primitives (Ed25519, ML-DSA-65, X25519, ML-KEM-768, AES-256-GCM,
  HKDF)
- ✅ Self-signed cert generation via `rcgen` (function exists, not wired in)
- ✅ Database (SQLite WAL, migrations, tenant CRUD, quotas, tokens)
- ✅ Image DSL parser + Containerfile generator (but not the actual build)
- ✅ CLI subcommand structure (tenant / credentials / agent-token / image /
  worker / audit / keys / init)

---

## License

Apache-2.0. See [LICENSE](LICENSE).
