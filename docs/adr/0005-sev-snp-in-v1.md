# ADR 0005: SEV-SNP in v1, not deferred

## Status

Accepted

## Context

Stronghold's original threat model (v1.0-v3.0) treated Vultr hypervisor compromise as out of scope. The audit log + WebAuthn + PQ crypto protect against network adversaries and post-hoc quantum adversaries, but **none of them protect against the Vultr hypervisor reading the gateway's memory at runtime** — which is exactly where the audit signing keys, the WebAuthn challenge store, and the unsealed agent tokens live.

## Decision

Include **SEV-SNP (AMD Secure Encrypted Virtualization - Secure Nested Paging)** as a first-class v1 feature, not a future addition.

## Alternatives Considered

### Defer SEV-SNP to v2
- **Pros:** Faster v1 release, simpler deployment
- **Cons:** The entire cryptographic architecture is sound on paper and bypassable by anyone with hypervisor access. This is a false sense of security.

### Use Intel SGX instead
- **Pros:** More widely available
- **Cons:** SGX has had numerous side-channel vulnerabilities (Foreshadow, L1TF, SGAxe, etc.). SGX enclaves have limited memory (typically 128MB-512MB EPC). SEV-SNP protects the entire VM.

### Use Intel TDX
- **Pros:** Intel's equivalent of SEV-SNP
- **Cons:** Not yet widely available on Vultr. SEV-SNP is available now.

### Don't use a TEE at all
- **Pros:** Simplest, no performance overhead
- **Cons:** Hypervisor can read all memory. Keys, tokens, and challenge state are all exposed.

## Consequences

### Positive
- Hypervisor cannot read gateway memory (encrypted by AMD hardware)
- Hypervisor cannot modify gateway memory (integrity protection)
- Phone can verify (via attestation) that the gateway runs on genuine SEV-SNP hardware
- Audit keys are sealed to the launch measurement — if the binary is modified, keys cannot be unsealed
- Closes the biggest gap in the threat model

### Negative
- ~5-10% CPU overhead from memory encryption
- ~30 second cold-boot overhead (SEV-SNP launch + attestation)
- ~5% memory overhead for integrity metadata
- Requires SEV-SNP-capable Vultr plan (~$20-40/month extra)
- Adds complexity (attestation flow, key sealing, measurement verification)

### Neutral
- `sev` crate is Rust-native and well-maintained
- SEV-SNP is behind a cargo feature flag (`--features sev-snp`), so dev/eval environments can opt out

## Implementation

### Cargo feature

```toml
[features]
default = ["sev-snp"]
sev-snp = ["dep:sev"]
no-sev-snp = []
```

### Runtime check

```rust
pub fn verify_sev_snp_available() -> Result<()> {
    if !std::path::Path::new("/dev/sev").exists() {
        return Err(anyhow::anyhow!("SEV-SNP not available"));
    }
    Ok(())
}
```

### Attestation endpoint

```
GET /attestation → { report, measurement, sev_snp_active, ... }
```

The phone verifies the measurement before enrolling credentials.

### Key sealing

```rust
pub fn seal_keys(keys: &[u8]) -> Result<Vec<u8>> {
    // Sealed to current SEV-SNP measurement
    // Can only be unsealed when running with the exact same binary
}
```

## References

- [AMD SEV-SNP documentation](https://www.amd.com/en/processors/sev-secure-encrypted-virtualization)
- [sev crate (Rust)](https://docs.rs/sev)
- [Vultr SEV-SNP plans](https://www.vultr.com/docs/)
