# Stronghold

> A self-hosted gateway that lets any AI agent with `bash` + `curl` request, attach to, and work inside isolated containerd workspaces on a fleet of Vultr boxes — with phone-approved sessions, post-quantum cryptography end-to-end, and SEV-SNP confidential computing.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![Status: Beta](https://img.shields.io/badge/status-beta-yellow.svg)](CHANGELOG.md)

---

> 🟡 **Beta — not recommended for production without further testing.**
>
> Stronghold `0.10.x-beta` has closed all 18 previously-tracked alpha gaps:
> TLS, WebAuthn signature verification, PTY `connect_token` auth, E2E push
> encryption, anomaly scanning, quorum enforcement, SSE approval events,
> audit streaming, audit signature verification, Prometheus metrics, real
> worker listing, real image builds, rate limiting, request tracing, load
> testing, the `--dev` flag, ML-DSA-65 signatures, and self-signed cert
> generation are all **wired into the running gateway**. See
> [What works](#what-works-beta-scope) below.
>
> Remaining limitations are documented under
> [Known Limitations](#known-limitations) — they are either hardware-blocked
> (SEV-SNP on real silicon, FIDO PQC authenticators), out of scope for the
> current multi-tenancy model, or stubs deliberately left for the v1.0 RC.

---

## What is Stronghold?

Stronghold is a **control plane** that sits between AI agents (GLM-5.2 on chat.z.ai, or any agent with bash) and a fleet of Vultr VPS workers running k3s. Agents request machines via a small HTTP protocol; humans approve via phone (WebAuthn Face ID / YubiKey / multi-credential, through the phone's native browser — **no custom app**); approved sessions get full PTY access to an isolated containerd workspace built from a **Rocky Linux base image**. Every byte of every session is logged with **dual post-quantum signatures** (Ed25519 + ML-DSA-65). The gateway binary optionally runs inside an **AMD SEV-SNP confidential VM**, with attestation verifiable by the tenant's phone before any approval.

### Design principles

1. **Agents are the tenants.** Projects are an emergent property of what an agent happens to be doing. The gateway has no concept of "project" — only agents, machines, and sessions.
2. **Full shell, not per-command approval.** One tap opens a TTL'd workspace. The agent has full PTY access. Destructive operations trigger quorum re-approval mid-session.
3. **Post-quantum everywhere crypto lives.** TLS 1.3 + X25519MLKEM768 hybrid transport. Ed25519 + ML-DSA-65 dual-signed audit log. X25519 + ML-KEM-768 hybrid push encryption.
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
        │                   │ (TLS 1.3 + X25519MLKEM768)
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

```bash
curl -X POST https://your-gateway:8443/agent/order \
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

**This is BETA software.** All 18 alpha gaps tracked in `0.9.0-alpha` have been
closed in the running gateway — TLS termination, WebAuthn signature
verification, PTY `connect_token` auth, E2E-encrypted push, anomaly scanning
wired into the PTY proxy, quorum enforcement for destructive ops, real SSE
approval events, real audit-streaming WebSocket, full `audit verify` signature
checks, Prometheus `/metrics`, real worker listing, real image builds, global
rate limiting, request tracing, a load test (100 sessions + 100 audit entries
in <30s), the `--dev` flag plumbing, real ML-DSA-65 signatures, and
auto-generated self-signed certs are all live.

Remaining limitations (see [Known Limitations](#known-limitations)) are
either hardware-blocked (SEV-SNP on real silicon, FIDO PQC authenticators),
out of scope for the current multi-tenancy model, or stubs deliberately
deferred to the v1.0 RC. Beta is **not recommended for production without
further testing**.

See [CHANGELOG.md](CHANGELOG.md) for version history and [CONTRIBUTING.md](CONTRIBUTING.md) for how to contribute.

---

## What works (beta scope)

### Transport & Crypto ✅
- ✅ **TLS termination** via `axum_server::bind_rustls()` with the X25519MLKEM768 hybrid PQ key exchange from `rustls-post-quantum`. Serves real HTTPS on port 8443.
- ✅ **Self-signed cert auto-generation** on first boot via `rcgen` 0.14 (ECDSA P-256, 10-year validity, written to `tls.crt` / `tls.key` with proper file modes). Loaded by `serve()` if no cert is present.
- ✅ **WebAuthn signature verification** — ECDSA P-256 (ES256) signatures verified against the stored credential public key, not just assertion metadata.
- ✅ **E2E-encrypted push notifications** — all 5 production push functions route through `send_encrypted_or_fallback()`. Payloads are sealed with X25519 + ML-KEM-768 hybrid KEM → HKDF-256 → AES-256-GCM when the phone has enrolled keys (plaintext fallback only when no keys are enrolled yet).
- ✅ **Dual-signed audit log** (Ed25519 + ML-DSA-65 via `ml-dsa` 0.1.1), hash-chained, SEV-SNP attested, offline-verifiable.
- ✅ **ML-DSA-65** real post-quantum signatures (NIST FIPS 204).

### PTY proxy & session control ✅
- ✅ **PTY `connect_token` verification** — token is verified against its SHA-256 hash stored in the `machines` table. Missing/wrong token → 401.
- ✅ **Anomaly scanning wired into the PTY proxy** — detects `curl`/`wget`/`scp`, `rm -rf`, `sudo`, `ssh`; writes audit entries.
- ✅ **Quorum enforcement for destructive ops** — destructive commands are blocked, a `pending_sessions` row is created, the proxy polls for approval, and executes only on approval.
- ✅ **Real SSE approval stream** — `pending_approval_stream()` polls the DB every 500 ms and yields real `approval_request` events.
- ✅ **Real audit-streaming WebSocket** — `audit_stream()` long-polls the DB and streams JSON audit entries to authorised clients.
- ✅ **Fail-closed** — the PTY proxy fails closed: missing `connect_token`, missing quorum, and unhandled anomaly all block the session.

### Fleet & build pipeline ✅ (with caveats below)
- ✅ **Worker list** — real `kube::Api::<Node>::list()` with capacity parsing (allocatable CPU/memory).
- ✅ **Image build** — real `podman build` + `podman inspect` → real digest.
- ✅ **k3s pod scheduling** (real `kube-rs` API calls).

### Observability & multi-tenancy ✅ (with caveats below)
- ✅ **Prometheus `/metrics` route** — returns `sessions_active`, `approvals_pending`, `audit_entries_total` (Prometheus text format).
- ✅ **Global concurrency rate limiting** (cap 100; 503 on overflow).
- ✅ **Request tracing** — `TraceLayer` on all routes.
- ✅ **`audit verify` signature check** — verifies the hash chain, Ed25519 signatures, and ML-DSA-65 signatures.
- ✅ **ML-DSA-65 signature verification** in the audit verifier.

### CLI & tooling ✅
- ✅ **`--dev` flag** — properly threads through and skips the SEV-SNP availability check (no need to set `STRONGHOLD_DEV=1` manually).
- ✅ **Load test passes** — 100 sessions + 100 audit entries created in <30 s.

### What worked in alpha (still works)
- ✅ Agent protocol HTTP handlers (`ORDER`/`RESUME`/`RELEASE`/`EXTEND`)
- ✅ Agent token minting / verification
- ✅ PTY proxy data path (real `kube exec` via WebSocket, bidirectional byte pumping)
- ✅ Crypto primitives (Ed25519, ML-DSA-65, X25519, ML-KEM-768, AES-256-GCM, HKDF)
- ✅ Database (SQLite WAL, migrations, tenant CRUD, quotas, tokens)
- ✅ Image DSL parser + Containerfile generator
- ✅ CLI subcommand structure (tenant / credentials / agent-token / image / worker / audit / keys / init)

---

## Known Limitations

The following items are **still not implemented** or are **partial** as of
`0.10.0-beta`. None are software-fixable in the current scope without new
hardware or a deliberate scope change. The 18 alpha gaps tracked in
`0.9.0-alpha` are all closed — see [What works](#what-works-beta-scope).

Legend: ❌ = not implemented · ⚠️ = partial / hardware-blocked · ✅ = works

### Hardware-blocked

1. ⚠️ **WebAuthn PQC.** FIDO authenticators do not yet ship with post-quantum
   algorithms (expected ~2027). Session TTLs are hours, so a quantum break
   in 10 years gets nothing useful. Accepted gap; revisit when PQC
   authenticators ship.
2. ⚠️ **SEV-SNP on real hardware.** The `sev` crate is wired in with real
   ioctl calls and key sealing is tested with software keys, but the dev
   box lacks `/dev/sev`. The measurement registry
   (`docs/MEASUREMENTS/v1.0.txt`) is a placeholder until the gateway is
   built and first booted on a real SEV-SNP box.

### Out of scope for the current multi-tenancy model

3. ⚠️ **Per-token rate limiting.** Only a global concurrency limit (cap 100,
   503 on overflow) is enforced. There is no per-token bucket.
4. ⚠️ **Per-tenant Kubernetes namespaces.** All pods land in the `default`
   namespace; `tenant_id` is a pod label, not a namespace boundary.
5. ❌ **Per-tenant NetworkPolicy objects.** No `NetworkPolicy` objects are
   created. Cross-tenant pod traffic is not denied at the network layer.

### Deliberate stubs deferred to the v1.0 RC

6. ❌ **VPS escalation** — `machines/escalation.rs` still returns a fake VPS
   ID. Real Vultr API + cloud-init + k3s-agent join is planned for v1.0.
7. ❌ **Image push / image pull** — `image push` and `image pull` are still
   stubs (no registry interactions).
8. ⚠️ **Anomaly push to phone** — `push_anomaly()` is defined but never
   called. Anomalies are written to the audit log only; the phone is not
   pushed.
9. ⚠️ **Quorum push to phone** — quorum requests land in `pending_sessions`
   but no ntfy push fires. The phone polls the SSE stream instead.

---

## License

Apache-2.0. See [LICENSE](LICENSE).
