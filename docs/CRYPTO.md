# Stronghold Cryptography

This document describes the cryptographic primitives used by Stronghold,
the rationale for each choice, and the test coverage.

> ⚠️ **Alpha status note.** The crypto **primitives** below are implemented
> and unit-tested. However, not all of them are wired into the running
> gateway — see the "PQC Gaps" table for the wiring status of each layer.

## Algorithm Choices

| Layer | Algorithm | Crate | Rationale |
|---|---|---|---|
| Transport (TLS) | TLS 1.3 + X25519MLKEM768 hybrid | `rustls` 0.23 with `prefer-post-quantum` | Hybrid PQ key exchange; harvest-now-decrypt-later mitigation |
| Audit signatures | Ed25519 + ML-DSA-65 (dual) | `ed25519-dalek`, `ml-dsa` 0.1.1 | If either is broken, the other still proves authenticity |
| Push encryption (E2E) | X25519 + ML-KEM-768 hybrid KEM → HKDF-256 → AES-256-GCM | `x25519-dalek`, `ml-kem`, `hkdf`, `aes-gcm` | ntfy server sees ciphertext only |
| Session approval | WebAuthn (ES256/RS256/Ed25519) | `webauthn-rs` | Phishing-resistant, biometric-verified. PQC gap accepted (TTLs are hours). |
| Hashing | SHA-256 | `sha2` | Standard, sufficient for our threat model |
| Token generation | 32-byte CSPRNG (`OsRng`) | `rand` | Platform CSPRNG (getrandom/urandom) |

## Key Sizes

| Key | Size (bytes) |
|---|---|
| Ed25519 secret | 32 |
| Ed25519 public | 32 |
| Ed25519 signature | 64 |
| X25519 secret | 32 |
| X25519 public | 32 |
| X25519 shared secret | 32 |
| ML-KEM-768 decapsulation key | 2400 |
| ML-KEM-768 encapsulation key | 1184 |
| ML-KEM-768 ciphertext | 1088 |
| ML-KEM-768 shared secret | 32 |
| ML-DSA-65 secret (seed) | 32 |
| ML-DSA-65 public | 1952 |
| ML-DSA-65 signature | 3309 |
| AES-256-GCM key | 32 |
| AES-256-GCM nonce | 12 |
| HKDF output | 32 |
| WebAuthn challenge | 32 |
| Agent/phone tokens | 32 (base64url-encoded) |

## Hybrid Construction

### Audit Signatures (Ed25519 + ML-DSA-65)

Every audit log entry is signed with both Ed25519 and ML-DSA-65. The
`DualSignature` struct contains both base64-encoded signatures. Verification
requires BOTH to pass. If either algorithm is broken in the future, the
other still proves authenticity.

**Current state:** Both Ed25519 and ML-DSA-65 are fully implemented and tested.
ML-DSA-65 uses the `ml-dsa` 0.1.1 crate (NIST FIPS 204). Key sizes:
- Secret key (32-byte seed)
- Public key (1952 bytes)
- Signature (3309 bytes)

Backward compatibility: legacy Ed25519-only entries still verify.

> ⚠️ **`audit verify` CLI note.** The dual-signing **writer** is fully wired
> into the gateway, but the `stronghold audit verify` CLI currently **only
> checks the hash chain**. Ed25519 and ML-DSA-65 signature verification in the
> verifier is a TODO. See gap #16 in the README.

### Push Encryption (X25519 + ML-KEM-768 → HKDF → AES-256-GCM)

```
Gateway                                          Phone
  │                                               │
  │  encapsulate(phone_x25519_pub, phone_mlkem_pub)
  │    ├─ X25519 DH → x25519_shared (32 bytes)
  │    ├─ ML-KEM-768 encapsulate → mlkem_shared (32 bytes) + ct
  │    └─ HKDF-256(x25519_shared || mlkem_shared, info="stronghold-push-e2e-v1")
  │         → aes_key (32 bytes)
  │                                               │
  │  AES-256-GCM encrypt(payload, aes_key, nonce) │
  │  → ciphertext                                 │
  │                                               │
  │  ──── EncapsulatedSecret + nonce + ciphertext ────→
  │                                               │
  │                          decapsulate(phone_keys, encapsulated)
  │                                               ├─ X25519 DH → x25519_shared
  │                                               ├─ ML-KEM-768 decapsulate → mlkem_shared
  │                                               └─ HKDF-256(...) → aes_key
  │                                               │
  │                                               AES-256-GCM decrypt(ciphertext, aes_key, nonce)
  │                                               → payload
```

The ntfy server (running on the Vultr box) sees only ciphertext. Even if the
Vultr hypervisor is compromised, the push payload is protected by the hybrid
KEM — the attacker would need to break both X25519 and ML-KEM-768.

## Key Storage

All keys are stored on disk with mode 0600 (owner read/write only) in a
directory with mode 0700:

```
/var/lib/stronghold/keys/           # mode 0700
├── audit_ed25519.key               # mode 0600, 32 bytes
├── audit_ed25519.pub               # mode 0644, 32 bytes
├── audit_mldsa65.key               # mode 0600, 32 bytes (seed)
├── audit_mldsa65.pub               # mode 0644, 1952 bytes
├── push_x25519.key                 # mode 0600, 32 bytes
├── push_x25519.pub                 # mode 0644, 32 bytes
├── push_mlkem768.key               # mode 0600, 2400 bytes
├── push_mlkem768.pub               # mode 0644, 1184 bytes
├── tls.crt                         # mode 0644, PEM
└── tls.key                         # mode 0600, PEM
```

Secret files are written atomically: write to `<path>.tmp`, `fsync`, rename
to `<path>`. This prevents partial writes if the process is killed.

On SEV-SNP hardware, all keys are sealed to the launch measurement —
if the binary is modified, the keys cannot be unsealed. (SEV-SNP is untested
on real hardware as of `0.9.0-alpha` — see `docs/SEV_SNP.md`.)

## Test Coverage

| Module | Unit Tests | Property Tests | KAT Tests | Fuzz |
|---|---|---|---|---|
| `crypto/hybrid_sig.rs` | 22 | 4 | 3 (RFC 8032) | 1 (`audit_verify_chain` exercises dual-sig) |
| `crypto/hybrid_kem.rs` | 18 | 3 | 1 (RFC 7748) | 1 (`hybrid_kem_encapsulate`) |
| `crypto/tls.rs` | 7 | 0 | 0 | N/A |
| `crypto/webauthn.rs` | 24 | 4 | 0 | 1 (`webauthn_assertion_decode`) |
| **Total** | **71** | **11** | **4** | **3 crypto-related fuzz harnesses** (4 total in repo, incl. `image_toml_parse`) |

> Test counts reflect the unit-tested code paths. The `audit verify` CLI is
> tracked separately — its hash-chain check is tested, but signature
> verification of audit entries is a TODO (see gap #16).

### Known-Answer Tests

- **RFC 8032 §7.1** (Ed25519): 3 test vectors — secret→public derivation,
  message signing, signature verification. Vectors 1 and 2 check exact
  signature bytes; vector 3 checks round-trip (the published signature in
  some sources has a typo).
- **RFC 7748 §6.1** (X25519): basepoint KAT — derive Alice's public key from
  her private key via scalar multiplication with the X25519 basepoint.
- **RFC 5869 §A.1** (HKDF-SHA256): TODO — add KAT test.
- **ML-DSA-65** (NIST FIPS 204): sign/verify round-trip with the `ml-dsa` 0.1.1
  crate. KAT vectors against the official FIPS 204 sample signatures are a
  TODO — the crate's own test suite covers them, but Stronghold does not yet
  pin its own KAT file.

### Property Tests

- Ed25519: sign+verify round-trip on random messages, tampered message always
  fails, unique signatures per message, save+load round-trip.
- ML-DSA-65: sign+verify round-trip, tampered message fails, save+load
  round-trip (the seed is 32 bytes, the public key is 1952 bytes, the
  signature is 3309 bytes).
- ML-KEM-768: encapsulate+decapsulate round-trip, unique encapsulations,
  save+load round-trip.
- WebAuthn: challenge determinism, random challenge uniqueness.

## PQC Gaps

| Layer | PQC Status | Mitigation |
|---|---|---|
| Transport | ⚠️ Code complete, **not wired into server** | `crypto/tls.rs` builds the X25519MLKEM768 hybrid config, but `main.rs::serve()` binds a plain TCP listener and serves HTTP. The TLS config is computed and discarded (`let _tls_config = ...`). All gateway traffic is plaintext until TLS is wired into startup. Use Tailscale/WireGuard to compensate in dev. |
| Audit signatures | ✅ Real ML-DSA-65 (via `ml-dsa` 0.1.1) | None needed. (Caveat: `audit verify` CLI only checks the hash chain — signature verification is TODO.) |
| Push encryption | ⚠️ Code complete, **not wired into production paths** | Hybrid KEM + AES-256-GCM primitives are implemented. Only the test-only `send_encrypted_notification_to()` uses them. All production push paths send plaintext. |
| WebAuthn | ❌ Classical only (and signature not verified) | Session TTLs are hours; quantum break in 10 years gets nothing useful. Revisit ~2027 when FIDO ships PQC authenticators. **Additionally**, the current WebAuthn verifier does not check the cryptographic signature — only the assertion metadata. |

## References

- [NIST FIPS 203: ML-KEM](https://csrc.nist.gov/pubs/fips/203/final)
- [NIST FIPS 204: ML-DSA](https://csrc.nist.gov/pubs/fips/204/final)
- [RFC 8032: Ed25519](https://datatracker.ietf.org/doc/html/rfc8032)
- [RFC 7748: X25519](https://datatracker.ietf.org/doc/html/rfc7748)
- [RFC 5869: HKDF](https://datatracker.ietf.org/doc/html/rfc5869)
- [RFC 8446: TLS 1.3](https://datatracker.ietf.org/doc/html/rfc8446)
- [W3C WebAuthn](https://www.w3.org/TR/webauthn/)
- [ADRs](adr/): 0004 (PQ hybrid), 0005 (SEV-SNP), 0006 (WebAuthn classical)
