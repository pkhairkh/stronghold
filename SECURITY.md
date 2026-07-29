# Security Policy

> ⚠️ **WARNING: ALPHA-QUALITY — DO NOT DEPLOY IN PRODUCTION** ⚠️
>
> Stronghold `0.9.x-alpha` is alpha-stage software. Several security-critical
> features advertised in this document are **NOT enforced by the running
> gateway**: TLS is not enabled, WebAuthn signature verification is not
> implemented, quorum is not enforced, push notifications are not E2E-encrypted
> in production paths, per-tenant network policies are not created, and SEV-SNP
> has never been tested on real hardware. The PTY proxy fails open. See the
> status indicators below and the [Known Limitations](README.md#known-limitations)
> list in the README before relying on any of these properties.
>
> This release is suitable for **local development, protocol experimentation,
> and threat-model review only.** Do not expose it to untrusted networks or
> store sensitive data behind it.

## Supported Versions

| Version         | Supported          |
|-----------------|--------------------|
| 0.9.x-alpha     | :warning: (alpha — security fixes only, see gaps below) |
| < 0.9.x         | :x: (unsupported scaffold) |

## Reporting a Vulnerability

If you discover a security vulnerability in Stronghold, please **do not** open a public GitHub issue.

Instead, please email **security@stronghold.dev** with:
1. A description of the vulnerability
2. Steps to reproduce
3. Potential impact
4. Any suggested fixes (optional)

### Response Timeline

- **Acknowledgment**: Within 48 hours
- **Initial Assessment**: Within 7 days
- **Fix or Mitigation**: Within 90 days (severity-dependent)
- **Public Disclosure**: After fix is released, coordinated with reporter

## Security Architecture

Stronghold's security model is documented in:
- [Threat Model](docs/THREAT_MODEL.md)
- [SEV-SNP Attestation](docs/SEV_SNP.md)
- [ADRs](docs/adr/)

### Key Security Properties

Legend: ✅ = works · ⚠️ = code exists but is not wired in · ❌ = not implemented

| # | Property | Status | Notes |
|---|----------|--------|-------|
| 1 | **Post-Quantum Transport** (TLS 1.3 + X25519MLKEM768 hybrid) | ⚠️ | TLS config is built in `crypto/tls.rs` but `main.rs::serve()` binds a plain TCP listener and discards it (`let _tls_config = ...`). Gateway serves **HTTP**, not HTTPS. |
| 2 | **Dual-Signed Audit** (Ed25519 + ML-DSA-65 on every entry) | ✅ | Real `ml-dsa` 0.1.1 signatures; hash-chained; offline-verifiable. |
| 3 | **SEV-SNP Confidential Computing** | ⚠️ | `sev` crate wired in with real ioctl calls; key sealing + attestation report generation implemented. **Never tested on real SEV-SNP hardware** — dev box lacks `/dev/sev-guest`. Measurement registry is a placeholder. |
| 4 | **WebAuthn Session Approval** | ⚠️ | Challenge generation and assertion **metadata** verification are implemented (challenge / origin / UV flag / RP ID hash). **The cryptographic signature is never verified.** Anyone who can craft a syntactically valid assertion blob can approve any session. |
| 5 | **Quorum for Destructive Ops** | ❌ | Data structures exist in `sessions/scopes.rs` but the PTY proxy does not scan commands or block. Destructive commands run freely. |
| 6 | **No External Providers for Content** | ✅ | ntfy is self-hosted; APNs/FCM are wake-up only. |
| 7 | **Fail Closed** | ⚠️ | Agent token verification, ORDER without approval, and missing-credential paths fail closed. **The PTY proxy fails open**: it does not verify `connect_token`, does not enforce quorum, and does not run the anomaly scanner. Anyone with the WS URL can attach to any session. |

### Other known security gaps (alpha scope)

- ❌ **Per-tenant Kubernetes namespaces** — all pods land in `default`; `tenant_id` is only a label.
- ❌ **Per-tenant NetworkPolicy objects** — never created; cross-tenant pod traffic is not denied at the network layer.
- ❌ **Push notifications are NOT E2E-encrypted in production** — only the test-only `send_encrypted_notification_to()` encrypts payloads.
- ❌ **Prometheus metrics endpoint** — no `/metrics` route exists.
- ❌ **`audit verify` signature check** — CLI only verifies the hash chain; signature verification is TODO.
- ❌ **`--dev` flag** — does not bypass SEV-SNP check in `serve()` (sets a struct field, not `STRONGHOLD_DEV`). Use `STRONGHOLD_DEV=1`.

See [CHANGELOG.md](CHANGELOG.md#known-open-gaps-alpha-scope--advertised-but-not-enforced-in-the-running-gateway) for the exhaustive list.

## Security Considerations for Operators

If you are running Stronghold (development only — do not run in production yet):

1. **Verify SEV-SNP Measurement**: Before enrolling credentials, verify the `SEV_SNP_MEASUREMENT` matches the published measurement in `docs/MEASUREMENTS/`. *Note: `docs/MEASUREMENTS/v1.0.txt` is currently an all-zero placeholder; SEV-SNP has not been tested on real hardware.*
2. **Rotate Keys Regularly**: Use `stronghold keys rotate-audit` periodically.
3. **Review Audit Logs**: Run `stronghold audit verify` regularly to detect tampering. *Note: as of alpha, this only checks the hash chain — signature verification is TODO.*
4. **Keep Backups**: Use `stronghold backup` to encrypt and store key material off-box.
5. **Harden SSH**: Disable password auth, use ed25519 keys only, install fail2ban.
6. **Network Isolation**: Use Tailscale for worker-to-control-plane communication. *Note: TLS is not enabled on the gateway — assume the network is untrusted and use a transport-level VPN (Tailscale/WireGuard) to compensate.*
7. **Do NOT expose port 8443 to the public internet** — the gateway serves plain HTTP and the PTY WebSocket does not verify `connect_token`.
