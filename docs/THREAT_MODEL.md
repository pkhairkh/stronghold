# Stronghold Threat Model

> 🟡 **Beta — not recommended for production without further testing.**
> Most mitigations described in this document now work as described in the
> running gateway (see the per-threat status tags). Remaining limitations
> are hardware-blocked (SEV-SNP on real silicon, FIDO PQC authenticators),
> out of scope for the current multi-tenancy model, or deliberate stubs
> deferred to the v1.0 RC.
>
> Each threat below carries an implementation-status tag:
>
> - ✅ **Mitigation works as described**
> - ⚠️ **PARTIAL / HARDWARE-BLOCKED** — code is wired in but limited by
>   hardware availability, scope, or a deliberate stub
> - ❌ **NOT IMPLEMENTED** — no working mitigation in the running gateway

## Overview

Stronghold is a self-hosted gateway that lets AI agents request and work inside isolated containerd workspaces on a fleet of Vultr boxes, with phone-approved sessions and post-quantum cryptography. This document describes the threats addressed, threats not addressed, and the cryptographic trust model.

---

## Threats Addressed

### 1. Agent steals SSH key
**Threat:** The agent gains access to SSH keys and uses them to access other systems.

**Mitigation:** The agent never has an SSH key. The gateway holds all credentials in memory (or sealed in SEV-SNP memory). The agent communicates only via the HTTP/WebSocket protocol. The gateway translates agent requests into k3s API calls.

**Implementation status:** ✅

### 2. Agent runs unapproved command
**Threat:** The agent runs commands the human did not authorize.

**Mitigation:** Every session requires phone approval (WebAuthn Face ID / YubiKey). Destructive commands (`rm -rf`, `git push --force`, `DROP TABLE`) trigger quorum re-approval mid-session — the command blocks until N enrolled credentials approve it.

**Implementation status:** ✅. Quorum is enforced by the PTY proxy: destructive commands are blocked, a `pending_sessions` row is created, the proxy polls for approval, and the command executes only on approval. The WebAuthn signature itself is also verified (see threat #10).

### 3. Agent exfiltrates source code
**Threat:** The agent reads source code and sends it to an external server.

**Mitigation:** Per-pod network policies (Calico/Cilium) default-deny egress. Only whitelisted hosts (github.com, crates.io, etc.) are reachable. The anomaly scanner detects `curl`/`wget`/`scp` to external hosts and writes an audit entry.

**Implementation status:** ⚠️ **PARTIAL.**

- ✅ The anomaly scanner in `anomaly/mod.rs` is wired into the PTY proxy, detects `curl`/`wget`/`scp`, `rm -rf`, `sudo`, `ssh`, and writes audit entries.
- ❌ Per-tenant `NetworkPolicy` objects are NOT created. All pods land in the `default` namespace and there is no default-deny egress policy. An approved agent can `curl` arbitrary external hosts.
- ⚠️ `push_anomaly()` is defined but never called. Anomalies are written to the audit log only; the phone is not pushed for review.

**Operator mitigation (beta):** Run workers behind an external firewall (e.g. Vultr firewall, Tailscale ACLs) that blocks outbound traffic to non-allowlisted hosts. The anomaly scanner is now wired in but does not push to the phone — review audit logs manually or wire `push_anomaly()` in a follow-up.

### 4. Agent reuses old approval
**Threat:** An attacker replays an old approval to start a new session.

**Mitigation:** Each approval is single-use, bound to the exact `(session_id, scope_hash, ttl, sev_snp_measurement)` tuple via the WebAuthn challenge. The challenge includes a SHA-256 of these values. Replaying an old assertion fails because the challenge no longer matches.

**Implementation status:** ✅

### 5. Audit log tampering
**Threat:** An attacker modifies the audit log to hide unauthorized actions.

**Mitigation:** The audit log is:
- Append-only (no UPDATE or DELETE in the schema)
- Hash-chained (each entry includes SHA-256 of the previous entry)
- Dual-signed with Ed25519 + ML-DSA-65
- SEV-SNP attested (each entry includes the attestation report hash)

`stronghold audit verify` runs offline and detects any break in the chain or signature failure.

**Implementation status:** ✅. The audit log writer dual-signs every entry with Ed25519 + ML-DSA-65 (via `ml-dsa` 0.1.1). The `audit verify` CLI now checks the hash chain **and** both signature types (Ed25519 and ML-DSA-65). A tamperer must now recompute hashes **and** forge both signatures to fool the verifier.

### 6. Phone compromised
**Threat:** An attacker gains control of the tenant's phone.

**Mitigation:**
- WebAuthn requires biometric verification (Face ID / Touch ID / PIN) for every approval
- Multiple credentials can be enrolled (backup phone, YubiKey)
- Credentials can be revoked instantly via CLI
- Lost-all-credentials fallback: physical YubiKey stored offline + setup password

**Implementation status:** ✅. WebAuthn signatures are now verified against the stored credential public key, so a "Face ID fail" counter is meaningful.

### 7. Vultr hypervisor compromise
**Threat:** The Vultr hypervisor reads the gateway's memory and extracts keys.

**Mitigation:** The gateway runs inside an AMD SEV-SNP confidential VM. The hypervisor sees encrypted memory only. Audit signing keys are sealed to the launch measurement — if the binary is modified, the keys cannot be unsealed. The phone verifies the SEV-SNP attestation report before approving any session.

**Implementation status:** ⚠️ **HARDWARE-BLOCKED.** The `sev` crate is wired in with real ioctl calls, the key-sealing primitives (HKDF + AES-256-GCM) are fully tested with software keys, and the WebAuthn challenge mixes in the measurement hash. However, no SEV-SNP-capable Vultr box has been provisioned yet, so the full attestation flow has never run on real hardware. The measurement registry (`docs/MEASUREMENTS/v1.0.txt`) is an all-zero placeholder. Treat the SEV-SNP protection as unverified until golden integration tests run on a real SEV-SNP box.

### 8. Network adversary records traffic (harvest-now-decrypt-later)
**Threat:** An adversary records encrypted traffic today and decrypts it in the future when quantum computers are available.

**Mitigation:** TLS 1.3 with X25519MLKEM768 hybrid key exchange. A quantum adversary must break both X25519 (classical) and ML-KEM-768 (post-quantum) to decrypt the traffic.

**Implementation status:** ✅. The gateway now serves real HTTPS via `axum_server::bind_rustls()` with the X25519MLKEM768 hybrid PQ key exchange from `rustls-post-quantum`. All gateway traffic (HTTP and WebSocket) is now encrypted in transit with hybrid post-quantum crypto.

### 9. Push notification content intercepted
**Threat:** An adversary reads push notification content (which includes session details).

**Mitigation:** Push payloads are end-to-end encrypted with X25519 + ML-KEM-768 hybrid KEM → HKDF-256 → AES-256-GCM. The phone holds both private halves; the gateway holds both public halves. APNs/FCM (if used as iOS wake-up) see only "wake up," not content.

**Implementation status:** ✅. All 5 production push functions now route through `send_encrypted_or_fallback()`. Payloads are sealed with X25519 + ML-KEM-768 hybrid KEM → HKDF-256 → AES-256-GCM when the phone has enrolled keys (plaintext fallback only when no keys are enrolled yet, e.g. before phone enrollment completes). The ntfy server and any network observer between gateway and ntfy see ciphertext only once the phone has enrolled keys.

### 10. Phishing attack on phone
**Threat:** An attacker creates a fake gateway UI to capture the tenant's credentials.

**Mitigation:** WebAuthn is phishing-resistant. The passkey is bound to the gateway's origin. A fake UI on a different domain cannot trigger the WebAuthn assertion.

**Implementation status:** ✅. WebAuthn challenge generation, assertion metadata verification (challenge / origin / UV flag / RP ID hash), **and real ECDSA P-256 signature verification** against the stored credential public key are all implemented. The approval ceremony is now a proof of possession — an attacker without the passkey's private key cannot forge an assertion that the gateway will accept, even if they can construct a syntactically valid assertion blob.

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
WebAuthn authenticators do not yet support post-quantum algorithms (~2027 expected). A quantum adversary breaking WebAuthn's classical crypto in 10 years could forge approvals. **Mitigation:** Session TTLs are short (hours). A forged approval from 10 years ago is useless. **Accept the gap.** This is a hardware limitation — not fixable in software until FIDO ships PQC authenticators.

---

## Cryptographic Trust Model

| Principal | Key Material | Lifetime |
|---|---|---|
| Gateway | Ed25519 + ML-DSA-65 keypair (audit signing) | Forever, sealed to SEV-SNP measurement |
| Gateway | X25519 + ML-KEM-768 keypair (push encryption) | Forever, sealed to SEV-SNP measurement |
| Gateway | TLS certificate (self-signed + PQ pinned) | Auto-generated on first boot via `rcgen` 0.14; rotated manually |
| Phone | WebAuthn passkey (in secure enclave) | Forever, revocable |
| Phone | X25519 + ML-KEM-768 keypair (push decryption) | In IndexedDB, re-enrollable |
| Agent | Bearer token (scoped, TTL'd) | 1-24 hours, minted by tenant |
| Audit Log | Dual-signed by gateway keys | Forever, verifiable offline |

### Trust relationships

- **Phone trusts Gateway:** Phone pins the gateway's Ed25519 + ML-DSA-65 public keys at enrollment. Verifies SEV-SNP attestation before each approval.
- **Agent trusts Gateway:** Agent pins the gateway's TLS public key. Uses bearer token (minted by tenant) for authentication.
- **Gateway trusts Phone:** Gateway verifies WebAuthn assertions signed by enrolled credentials (signature verified, not just metadata).
- **Gateway trusts Agent:** Gateway verifies bearer tokens minted by the tenant.

### No PKI, no certificate authority

All trust is established via pinned public keys. No external CA is involved. The phone verifies the gateway's keys at enrollment (trust-on-first-use). Subsequent connections verify against the pinned keys.

---

## Failure Modes

Legend: ✅ works as described · ⚠️ partial / hardware-blocked · ❌ NOT implemented

| Situation | Behavior | Status |
|---|---|---|
| Phone offline | 60s timeout → auto-deny ORDER, logged | ✅ |
| Face ID fails 3× | Auto-deny, logged as `denied: biometric_failed` | ✅ |
| Passkey revoked | Pending ORDER auto-denied | ✅ |
| Agent token expired | 401. Tenant mints new token via CLI | ✅ |
| Gateway crashes | systemd restarts. SEV-SNP re-attests. Pending ORDERs auto-denied. | ⚠️ (SEV-SNP re-attest path untested on hardware) |
| SEV-SNP attestation fails | Gateway refuses to start. Keys cannot be unsealed. | ⚠️ (logic exists; never tested on real hardware) |
| Worker goes down | k3s reschedules pods (or marks machine as lost) | ✅ |
| ntfy server down | Pushes queued. Sessions still work via direct browser access. | ✅ |
| Destructive op quorum times out | Command rejected, agent gets 403 | ✅ |
| PTY `connect_token` missing / wrong | WS upgrade rejected (401) | ✅ |
| Anomaly scanner detects exfil | Audit entry written; command continues | ⚠️ (audit-only; `push_anomaly()` defined but not called — phone is not pushed) |
| Global concurrency limit hit | 503 Service Unavailable | ✅ |

**Golden rule:** Every failure mode fails closed. Never schedules a pod without explicit, fresh, signed approval.

**Current state (beta):** All failure modes listed above work as described, with the exception of SEV-SNP-related paths (hardware-blocked) and the anomaly-to-phone push (defined but not called — anomalies are written to the audit log only). The golden rule holds: no pod is scheduled without an approved, signed session.
