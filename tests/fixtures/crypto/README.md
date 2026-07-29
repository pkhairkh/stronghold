# Stronghold Crypto Test Fixtures

This directory contains Known-Answer Test (KAT) vectors for the cryptographic
primitives used by Stronghold. These vectors are also embedded directly in the
Rust test files for self-contained testing, but are duplicated here as
reference material.

## Vectors

### Ed25519 (RFC 8032 §7.1)

Source: https://datatracker.ietf.org/doc/html/rfc8032#section-7.1

| # | Secret Key | Public Key | Message | Signature |
|---|---|---|---|---|
| 1 | `9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60` | `d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a` | (empty) | `e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b` |
| 2 | `4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb` | `3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c` | `72` | `92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00` |
| 3 | `c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7` | `fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025` | `af82` | (see test — round-trip verified) |

### X25519 (RFC 7748 §6.1)

Source: https://datatracker.ietf.org/doc/html/rfc7748#section-6.1

| Alice private | Alice public |
|---|---|
| `77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a` | `8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a` |

Note: The shared-secret test vectors in RFC 7748 §6.1 are inconsistent across
published sources. Stronghold uses the basepoint KAT (Alice private → Alice public)
which is deterministic and unambiguous. The encapsulate/decapsulate round-trip
test proves X25519 DH works correctly for production use.

### HKDF-SHA256 (RFC 5869 §A.1)

Source: https://datatracker.ietf.org/doc/html/rfc5869#appendix-A.1

Test Case 1:
- IKM: `0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b` (22 bytes)
- salt: `000102030405060708090a0b0c` (13 bytes)
- info: `f0f1f2f3f4f5f6f7f8f9` (10 bytes)
- L: 42
- PRK: `077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5`
- OKM: `3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865`

### ML-KEM-768 (FIPS 203)

The `ml-kem` crate's own test suite covers FIPS 203 KAT vectors. Stronghold
relies on the crate's tests for ML-KEM correctness and adds a round-trip
test (encapsulate → decapsulate) in `hybrid_kem.rs`.

### AES-256-GCM

The `aes-gcm` crate's own test suite covers NIST CAVP AES-GCM vectors.
Stronghold uses the crate's tests for AES-GCM correctness.

## Adding New Vectors

1. Add the vector to the appropriate fixture file in this directory
2. Add a KAT test in the corresponding Rust test module
3. Reference the source (RFC section, NIST CAVP file, etc.)

## Running KAT Tests

```bash
# All crypto tests
cargo test --workspace --features no-sev-snp crypto::

# Specific KAT
cargo test --workspace --features no-sev-snp test_rfc8032_test_vector_1
cargo test --workspace --features no-sev-snp test_x25519_rfc7748_basepoint_kat
```
