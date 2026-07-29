# Stronghold Threat Model

> ⚠️ **ALPHA QUALITY — DO NOT DEPLOY IN PRODUCTION.** Several mitigations
> described in this document are NOT implemented or are NOT wired into the
> running gateway. Each threat below carries an implementation-status tag:
>
> - ✅ **Mitigation works as described**
> - ⚠️ **IMPLEMENTED BUT NOT WIRED IN** — code exists, but the running gateway
>   does not exercise it
> - ❌ **NOT IMPLEMENTED** — no working mitigation in the running gateway
>
> Do not rely on a mitigation marked ⚠️ or ❌ for actual security.

## Overview

Stronghold is a self-hosted gateway that lets AI agents request and work inside isolated containerd workspaces on a fleet of Vultr boxes, with phone-approved sessions and post-quantum cryptography. This document describes the threats addressed, threats not addressed, and the cryptographic trust model.

---

## Threats Addressed

### 1. Agent steals SSH key
**Threat:** The agent gains access to SSH keys and uses them to access other systems.

**Mitigation:** The agent never has an SSH key. The gateway holds all credentials in memory (or sealed in SEV-SNP memory). The agent communicates only via the HTTP/WebSocket protocol. The gateway translates agent requests into k3s API calls.

### 2. Agent runs unapproved command
**Threat:** The agent runs commands the human did not authorize.

**Mitigation:** Every session requires phone approval (WebAuthn Face ID / YubiKey). Destructive commands (`rm -rf`, `git push --force`, `DROP TABLE`) trigger quorum re-approval mid-session — the command blocks until N enrolled credentials approve it.

**Implementation status:** ❌ **NOT IMPLEMENTED.** Quorum data structures exist in `sessions/scopes.rs` but nothing calls them. The PTY proxy does not scan commands and does not block. Destructive commands run freely within an approved session. Additionally, the WebAuthn signature itself is NOT verified (only metadata is checked) — see threat #10 below and gap #2 in the README.

**Operator mitigation (alpha):** Only approve sessions for trusted agents. Assume that anything inside an approved PTY can be run, including destructive commands.

### 3. Agent exfiltrates source code
**Threat:** The agent reads source code and sends it to an external server.

**Mitigation:** Per-pod network policies (Calico/Cilium) default-deny egress. Only whitelisted hosts (github.com, crates.io, etc.) are reachable. The anomaly scanner detects `curl`/`wget`/`scp` to external hosts and pushes the phone for review.

**Implementation status:** ⚠️ **IMPLEMENTED BUT NOT WIRED IN.**

- The anomaly scanner in `anomaly/mod.rs` is implemented and unit-tested, but the PTY proxy never instantiates or calls it.
- Per-tenant `NetworkPolicy` objects are NOT created. All pods land in the `default` namespace and there is no default-deny egress policy. An approved agent can `curl` arbitrary external hosts.

**Operator mitigation (alpha):** Run workers behind an external firewall (e.g. Vultr firewall, Tailscale ACLs) that blocks outbound traffic to non-allowlisted hosts. Do not assume the gateway enforces egress.

### 4. Agent reuses old approval
**Threat:** An attacker replays an old approval to start a new session.

**Mitigation:** Each approval is single-use, bound to the exact `(session_id, scope_hash, ttl, sev_snp_measurement)` tuple via the WebAuthn challenge. The challenge includes a SHA-256 of these values. Replaying an old assertion fails because the challenge no longer matches.

### 5. Audit log tampering
**Threat:** An attacker modifies the audit log to hide unauthorized actions.

**Mitigation:** The audit log is:
- Append-only (no UPDATE or DELETE in the schema)
- Hash-chained (each entry includes SHA-256 of the previous entry)
- Dual-signed with Ed25519 + ML-DSA-65
- SEV-SNP attested (each entry includes the attestation report hash)

`stronghold audit verify` runs offline and detects any break in the chain or signature failure.

**Implementation status:** ✅ for the audit log writer and dual-signing primitives. ⚠️ for `audit verify` — the CLI **only checks the hash chain**; Ed25519 and ML-DSA-65 signature verification is a TODO. A tamperer who can recompute SHA-256 hashes (e.g. someone with write access to the DB and the previous hash) can currently produce a chain that `audit verify` will accept. Full signature verification is planned.

### 6. Phone compromised
**Threat:** An attacker gains control of the tenant's phone.

**Mitigation:**
- WebAuthn requires biometric verification (Face ID / Touch ID / PIN) for every approval
- Multiple credentials can be enrolled (backup phone, YubiKey)
- Credentials can be revoked instantly via CLI
- Lost-all-credentials fallback: physical YubiKey stored offline + setup password

### 7. Vultr hypervisor compromise
**Threat:** The Vultr hypervisor reads the gateway's memory and extracts keys.

**Mitigation:** The gateway runs inside an AMD SEV-SNP confidential VM. The hypervisor sees encrypted memory only. Audit signing keys are sealed to the launch measurement — if the binary is modified, the keys cannot be unsealed. The phone verifies the SEV-SNP attestation report before approving any session.

**Implementation status:** ⚠️ **IMPLEMENTED BUT NOT WIRED IN / UNTESTED ON HARDWARE.** The `sev` crate is wired in with real ioctl calls, the key-sealing primitives (HKDF + AES-256-GCM) are fully tested on the dev box, and the WebAuthn challenge mixes in the measurement hash. However, no SEV-SNP-capable Vultr box has been provisioned yet, so the full attestation flow has never run on real hardware. The measurement registry (`docs/MEASUREMENTS/v1.0.txt`) is an all-zero placeholder. Treat the SEV-SNP protection as unverified until golden integration tests run on a real SEV-SNP box.

### 8. Network adversary records traffic (harvest-now-decrypt-later)
**Threat:** An adversary records encrypted traffic today and decrypts it in the future when quantum computers are available.

**Mitigation:** TLS 1.3 with X25519Kyber768Draft00 hybrid key exchange. A quantum adversary must break both X25519 (classical) and ML-KEM-768 (post-quantum) to decrypt the traffic.

**Implementation status:** ⚠️ **IMPLEMENTED BUT NOT WIRED IN.** The TLS config (rustls + `rustls-post-quantum`) is built in `crypto/tls.rs`, but `main.rs::serve()` binds a plain TCP listener and serves HTTP — the TLS config is computed and discarded. Until TLS is wired into server startup, **all gateway traffic is plaintext** and is harvestable by any network observer. Use a transport-level VPN (Tailscale/WireGuard) to compensate in dev.

### 9. Push notification content intercepted
**Threat:** An adversary reads push notification content (which includes session details).

**Mitigation:** Push payloads are end-to-end encrypted with X25519 + ML-KEM-768 hybrid KEM → HKDF-256 → AES-256-GCM. The phone holds both private halves; the gateway holds both public halves. APNs/FCM (if used as iOS wake-up) see only "wake up," not content.

**Implementation status:** ⚠️ **IMPLEMENTED BUT NOT WIRED IN.** The hybrid KEM + AES-256-GCM primitives are implemented and unit-tested. However, **only the test-only `send_encrypted_notification_to()` function uses them**. All production push paths send plaintext payloads through ntfy. The ntfy server (and any network observer between gateway and ntfy) can read session details in production. Treat push payloads as plaintext until production paths are migrated to use the encryption helper.

### 10. Phishing attack on phone
**Threat:** An attacker creates a fake gateway UI to capture the tenant's credentials.

**Mitigation:** WebAuthn is phishing-resistant. The passkey is bound to the gateway's origin. A fake UI on a different domain cannot trigger the WebAuthn assertion.

**Implementation status:** ⚠️ **PARTIAL.** WebAuthn challenge generation and assertion **metadata** verification are implemented (challenge / origin / UV flag / RP ID hash). **The cryptographic signature itself is NOT verified.** Anyone who can construct a syntactically valid assertion blob (correct CBOR structure with matching challenge/origin/UV/RP ID hash fields) can approve any session — without possessing the passkey's private key. This is more severe than phishing resistance; the WebAuthn approval ceremony is not currently a proof of possession. See gap #2 in the README.

---

## Threats NOT Addressed

### 1. Physical keylogger on phone
If the phone has a hardware keylogger, the attacker can capture the WebAuthn assertion. **Out of scope.** Mitigate by keeping phone OS updated.

### 2. Nation-state adversary
A nation-state with zero-day exploits for SEV-SNP, the phone OS, or the network stack can bypass all defenses. **Out of scope.** This is a personal/hobby project, not a defense against nation-states.

### 3. Vultr account compromise
If the attacker gains access to the Vultr account, they can reboot the box into a non-SEV-SNP mode. **Partial mitigation:** The phone verifies the SEV-SNP measurement before each approval. If the measurement changes (due to a modified binary), approvals fail. However, a sophisticated attacker could potentially roll back to a known-good measurement and extract keys before the tenant notices.

### 4. Supply chain attack on dependencies
If a dependency (Rust crate, ntfy, k3s, etc.) is compromised, the attacker can bypass Stronghold's defenses. **Mitigation:** Use `cargo audit` for Rust dependencies. Pin all versions in `Cargo.lock`. Verify binary signatures on downloads.

### 5. WebAuthn PQC gap
WebAuthn authenticators do not yet support post-quantum algorithms (~2027 expected). A quantum adversary breaking WebAuthn's classical crypto in 10 years could forge approvals. **Mitigation:** Session TTLs are short (hours). A forged approval from 10 years ago is useless. **Accept the gap.**

---

## Cryptographic Trust Model

| Principal | Key Material | Lifetime |
|---|---|---|
| Gateway | Ed25519 + ML-DSA-65 keypair (audit signing) | Forever, sealed to SEV-SNP measurement |
| Gateway | X25519 + ML-KEM-768 keypair (push encryption) | Forever, sealed to SEV-SNP measurement |
| Gateway | TLS certificate (self-signed + PQ pinned) | Rotated manually |
| Phone | WebAuthn passkey (in secure enclave) | Forever, revocable |
| Phone | X25519 + ML-KEM-768 keypair (push decryption) | In IndexedDB, re-enrollable |
| Agent | Bearer token (scoped, TTL'd) | 1-24 hours, minted by tenant |
| Audit Log | Dual-signed by gateway keys | Forever, verifiable offline |

### Trust relationships

- **Phone trusts Gateway:** Phone pins the gateway's Ed25519 + ML-DSA-65 public keys at enrollment. Verifies SEV-SNP attestation before each approval.
- **Agent trusts Gateway:** Agent pins the gateway's TLS public key. Uses bearer token (minted by tenant) for authentication.
- **Gateway trusts Phone:** Gateway verifies WebAuthn assertions signed by enrolled credentials.
- **Gateway trusts Agent:** Gateway verifies bearer tokens minted by the tenant.

### No PKI, no certificate authority

All trust is established via pinned public keys. No external CA is involved. The phone verifies the gateway's keys at enrollment (trust-on-first-use). Subsequent connections verify against the pinned keys.

---

## Failure Modes

Legend: ✅ works as described · ⚠️ partial / not wired in · ❌ NOT implemented

| Situation | Behavior | Status |
|---|---|---|
| Phone offline | 60s timeout → auto-deny ORDER, logged | ✅ |
| Face ID fails 3× | Auto-deny, logged as `denied: biometric_failed` | ❌ **NOT IMPLEMENTED** — the WebAuthn signature is not verified, so a "Face ID fail" counter is meaningless. Any syntactically valid assertion is accepted. |
| Passkey revoked | Pending ORDER auto-denied | ✅ |
| Agent token expired | 401. Tenant mints new token via CLI | ✅ |
| Gateway crashes | systemd restarts. SEV-SNP re-attests. Pending ORDERs auto-denied. | ⚠️ (SEV-SNP re-attest path untested on hardware) |
| SEV-SNP attestation fails | Gateway refuses to start. Keys cannot be unsealed. | ⚠️ (logic exists; never tested on real hardware) |
| Worker goes down | k3s reschedules pods (or marks machine as lost) | ✅ |
| ntfy server down | Pushes queued. Sessions still work via direct browser access. | ✅ |
| Destructive op quorum times out | Command rejected, agent gets 403 | ❌ **NOT IMPLEMENTED** — quorum is not enforced. Destructive commands run freely. |
| PTY `connect_token` missing / wrong | WS upgrade rejected | ❌ **NOT IMPLEMENTED** — the PTY WS does not verify `connect_token`. Anyone with the WS URL can attach to any session. |
| Anomaly scanner detects exfil | Command blocked, phone pushed for review | ❌ **NOT IMPLEMENTED** — scanner exists but is not wired into the PTY proxy. |

**Golden rule (target state):** Every failure mode fails closed. Never schedules a pod without explicit, fresh, signed approval.

**Current state (alpha):** Several failure modes (Face ID fail, destructive op quorum, PTY `connect_token` missing, anomaly detection) **do not have working mitigations** — see the ❌ rows above. The golden rule is the design intent, not the current behavior. Do not deploy in production.
