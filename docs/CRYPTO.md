# Stronghold Cryptography

This document describes the cryptographic primitives used by Stronghold,
the rationale for each choice, and the test coverage.

## Algorithm Choices

| Layer | Algorithm | Crate | Rationale |
|---|---|---|---|
| Transport (TLS) | TLS 1.3 + X25519MLKEM768 hybrid | `rustls` 0.23 with `prefer-post-quantum` | Hybrid PQ key exchange; harvest-now-decrypt-later mitigation |
| Audit signatures | Ed25519 + ML-DSA-65 (dual) | `ed25519-dalek`, `ml-dsa` (W1-T3 deferred) | If either is broken, the other still proves authenticity |
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
| ML-DSA-65 secret | (W1-T3 TBD) |
| ML-DSA-65 public | (W1-T3 TBD) |
| ML-DSA-65 signature | (W1-T3 TBD) |
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

**Current state (W1-T3 deferred):** Ed25519 is fully implemented and tested.
ML-DSA-65 is stubbed (empty signature). The `ml-dsa` crate's API is not yet
stable. Verification skips ML-DSA when the signature is empty (logged at
`warn` level). W1-T3 will replace the stub with real `ml_dsa` calls.

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
├── audit_mldsa65.key               # mode 0600, (W1-T3 TBD)
├── audit_mldsa65.pub               # mode 0644, (W1-T3 TBD)
├── push_x25519.key                 # mode 0600, 32 bytes
├── push_x25519.pub                 # mode 0644, 32 bytes
├── push_mlkem768.key               # mode 0600, 2400 bytes
├── push_mlkem768.pub               # mode 0644, 1184 bytes
├── tls.crt                         # mode 0644, PEM
└── tls.key                         # mode 0600, PEM
```

Secret files are written atomically: write to `<path>.tmp`, `fsync`, rename
to `<path>`. This prevents partial writes if the process is killed.

On SEV-SNP hardware (W7-T2), all keys are sealed to the launch measurement —
if the binary is modified, the keys cannot be unsealed.

## Test Coverage

| Module | Unit Tests | Property Tests | KAT Tests | Fuzz |
|---|---|---|---|---|
| `crypto/hybrid_sig.rs` | 14 | 4 | 3 (RFC 8032) | TODO W1-T11 |
| `crypto/hybrid_kem.rs` | 16 | 3 | 1 (RFC 7748) | TODO W1-T11 |
| `crypto/tls.rs` | 3 | 0 | 0 | N/A |
| `crypto/webauthn.rs` | 12 | 2 | 0 | TODO W1-T11 |
| **Total** | **45** | **9** | **4** | **0** |

### Known-Answer Tests

- **RFC 8032 §7.1** (Ed25519): 3 test vectors — secret→public derivation,
  message signing, signature verification. Vectors 1 and 2 check exact
  signature bytes; vector 3 checks round-trip (the published signature in
  some sources has a typo).
- **RFC 7748 §6.1** (X25519): basepoint KAT — derive Alice's public key from
  her private key via scalar multiplication with the X25519 basepoint.
- **RFC 5869 §A.1** (HKDF-SHA256): TODO — add KAT test.

### Property Tests

- Ed25519: sign+verify round-trip on random messages, tampered message always
  fails, unique signatures per message, save+load round-trip.
- ML-KEM-768: encapsulate+decapsulate round-trip, unique encapsulations,
  save+load round-trip.
- WebAuthn: challenge determinism, random challenge uniqueness.

## PQC Gaps

| Layer | PQC Status | Mitigation |
|---|---|---|
| Transport | ✅ X25519MLKEM768 hybrid | None needed |
| Audit signatures | ⚠️ Ed25519 only (ML-DSA-65 stubbed) | W1-T3 will add ML-DSA-65 |
| Push encryption | ✅ X25519 + ML-KEM-768 hybrid | None needed |
| WebAuthn | ❌ Classical only | Session TTLs are hours; quantum break in 10 years gets nothing useful. Revisit ~2027 when FIDO ships PQC authenticators. |

## References

- [NIST FIPS 203: ML-KEM](https://csrc.nist.gov/pubs/fips/203/final)
- [NIST FIPS 204: ML-DSA](https://csrc.nist.gov/pubs/fips/204/final)
- [RFC 8032: Ed25519](https://datatracker.ietf.org/doc/html/rfc8032)
- [RFC 7748: X25519](https://datatracker.ietf.org/doc/html/rfc7748)
- [RFC 5869: HKDF](https://datatracker.ietf.org/doc/html/rfc5869)
- [RFC 8446: TLS 1.3](https://datatracker.ietf.org/doc/html/rfc8446)
- [W3C WebAuthn](https://www.w3.org/TR/webauthn/)
- [ADRs](adr/): 0004 (PQ hybrid), 0005 (SEV-SNP), 0006 (WebAuthn classical)
