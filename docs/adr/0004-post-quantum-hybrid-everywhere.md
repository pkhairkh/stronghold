# ADR 0004: Post-quantum hybrid cryptography everywhere

## Status

Accepted

## Context

Stronghold handles sensitive operations:
- Agent commands (could include proprietary source code)
- Audit logs (must be verifiable forever)
- Push notifications (include session details)

A "harvest-now-decrypt-later" adversary records encrypted traffic today and decrypts it in the future when quantum computers become available. Classical cryptography (RSA, ECC) is vulnerable to Shor's algorithm.

## Decision

Use **post-quantum hybrid cryptography** everywhere crypto lives:

1. **Transport:** TLS 1.3 + X25519Kyber768Draft00 (hybrid key exchange)
2. **Audit signatures:** Ed25519 + ML-DSA-65 (dual-signed)
3. **Push encryption:** X25519 + ML-KEM-768 (hybrid KEM → AES-256-GCM)

## Alternatives Considered

### Pure classical (X25519, Ed25519 only)
- **Pros:** Fast, universally supported, well-understood
- **Cons:** Vulnerable to harvest-now-decrypt-later. In 10-15 years when quantum computers reach sufficient scale, all recorded traffic becomes decryptable.

### Pure post-quantum (ML-KEM, ML-DSA only)
- **Pros:** Forward-secure against quantum adversaries
- **Cons:** No cryptanalysis history (ML-KEM/ML-DSA standardized Aug 2024). Hardware authenticators don't support PQC yet. Risky to trust entirely.

### Hybrid (classical + post-quantum)
- **Pros:** Best of both worlds — if either scheme is broken, the other still protects. Belt and suspenders.
- **Cons:** Slightly more computation (two operations instead of one), larger signatures/ciphertexts.

## Consequences

### Positive
- Harvest-now-decrypt-later is mitigated
- If either classical or PQ algorithm is broken, the other still provides security
- Aligns with NIST FIPS 203 (ML-KEM) and FIPS 204 (ML-DSA) standards
- `rustls` already supports X25519Kyber768 (deployed by Cloudflare, Google, AWS since 2023)

### Negative
- Larger audit log entries (dual signatures)
- Slightly more CPU per crypto operation
- Phone needs WASM for ML-KEM (no browser-native support yet — ~12KB gzipped via `@noble/post-quantum`)

### Neutral
- Hybrid is the current best practice for PQC migration
- NIST recommends hybrid during the transition period

## Implementation

### Transport (TLS)
```toml
# gateway/Cargo.toml
[dependencies]
rustls = { version = "0.23", features = ["pqc-kyber", "ring"] }
```

### Audit signatures
```rust
// Every audit entry is signed with both Ed25519 and ML-DSA-65
pub struct DualSignature {
    pub sig_ed25519: String,
    pub sig_mldsa65: String,
}
```

### Push encryption
```rust
// Hybrid KEM: X25519 + ML-KEM-768
let (encapsulated, shared_secret) = encapsulate(phone_x25519_pub, phone_mlkem_pub)?;
let aes_key = derive_aes_key(&shared_secret, b"stronghold-push-v1");
let ciphertext = aes_256_gcm_encrypt(&aes_key, plaintext)?;
```

## References

- [NIST FIPS 203: ML-KEM](https://csrc.nist.gov/pubs/fips/203/final)
- [NIST FIPS 204: ML-DSA](https://csrc.nist.gov/pubs/fips/204/final)
- [rustls PQ support](https://rustls.org/)
- [RustCrypto ml-dsa](https://docs.rs/ml-dsa)
- [RustCrypto ml-kem](https://docs.rs/ml-kem)
- [@noble/post-quantum](https://github.com/paulmillr/noble-post-quantum)
