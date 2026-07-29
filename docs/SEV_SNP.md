# SEV-SNP Attestation Guide

## Overview

Stronghold's gateway runs inside an AMD SEV-SNP (Secure Encrypted Virtualization - Secure Nested Paging) confidential VM. This provides:

- **Encrypted RAM** — The hypervisor sees ciphertext only
- **Integrity protection** — The hypervisor cannot modify memory without detection
- **Attestation** — The phone can verify the gateway runs on genuine SEV-SNP hardware
- **Launch measurement binding** — Audit signing keys are sealed to the launch measurement

---

## Implementation Status (Wave 7 / v1.0)

| Component | Status | Notes |
|---|---|---|
| `tee/sev_snp.rs` — `verify_sev_snp_available()` | ✅ Real | Checks `/dev/sev-guest` (the guest-side device node) |
| `tee/sev_snp.rs` — `generate_attestation_report()` | ✅ Real (with dev fallback) | Uses `sev::firmware::guest::Firmware::get_report()` on real hardware; returns a stub report with `sev_snp_active: false` when `/dev/sev-guest` is absent |
| `tee/sev_snp.rs` — `current_measurement()` | ✅ Real (with dev fallback) | Reads the 48-byte `measurement` field from the firmware report; returns `None` on dev box |
| `tee/sev_snp.rs` — `seal_keys()` / `unseal_keys()` | ✅ Real (with dev fallback) | Uses `sev::firmware::guest::Firmware::get_derived_key()` with `GuestFieldSelect` bit 3 (measurement) on real hardware; falls back to HKDF-SHA256 from the measurement string + AES-256-GCM on dev |
| `tee/sealing.rs` — HKDF + AES-GCM primitives | ✅ Real, hardware-independent | Shared module compiled under both feature flags; fully unit-tested on the dev box |
| `tee/no_sev.rs` — non-TEE stub | ✅ Real stub | `sev_snp_active: false`, pass-through seal/unseal so the rest of the gateway runs without TEE |
| `crypto/webauthn.rs` — challenge includes SEV-SNP measurement hash | ✅ Real | New `generate_challenge_with_sev_snp()` mixes the measurement hash into the SHA-256 challenge; phone signs it; verify fails on measurement change |
| `routes/attestation.rs` — `/attestation` endpoint | ✅ Real | Returns `Json<AttestationReport>` from `generate_attestation_report()` |
| `docs/MEASUREMENTS/v1.0.txt` | 🚧 Placeholder | All-zero SHA-256 placeholder; replaced with the real measurement when built on SEV-SNP hardware |
| Wave 7 DoD: "Gateway boots inside SEV-SNP guest on real Vultr SEV box" | ❌ Blocked on W7-T1 | Provisioning a Vultr SEV-SNP box cannot be done from this environment; deferred to ops |

### What "real" means here

The `sev` crate (v4.0.0) provides a real Linux ioctl wrapper around `/dev/sev-guest`. The Stronghold code calls into it directly:

```rust,ignore
let mut fw = sev::firmware::guest::Firmware::open()?;   // opens /dev/sev-guest
let report = fw.get_report(None, None, Some(1))?;        // VMPL=1
let hw_key = fw.get_derived_key(None, DerivedKey::new(
    false,                                  // root_key_select = VCEK
    GuestFieldSelect(1 << 3),               // mix the launch measurement in
    0, 0, 0,
))?;
```

When `/dev/sev-guest` is absent (dev box, non-SEV Vultr plan, CI), `Firmware::open()` returns an `io::Error`, and the gateway falls back to a stub report so the rest of the code (audit log signing, push key generation, WebAuthn ceremonies) is fully exercised without TEE.

### What is still stubbed

- **Measurement registry** (`docs/MEASUREMENTS/v1.0.txt`) — the file ships with an all-zero placeholder. The real measurement is only known after the gateway is built and first booted inside an SEV-SNP guest. W7-T1 (provision Vultr SEV box) is the blocker.
- **GPG-signed measurement file** — `v1.0.txt.sig` will be produced by the release process once the real measurement is captured.
- **Phone-side attestation verification** (W7-T4) — the phone fetches `/attestation`, displays the measurement, and refuses enrollment on mismatch. This is wired into the enrollment HTML but the comparison logic against `docs/MEASUREMENTS/v1.0.txt` is implemented in the browser, not in this Rust crate.

---

## Why SEV-SNP?

Without SEV-SNP, the Vultr hypervisor can read the gateway's memory at runtime — which is exactly where the audit signing keys, the WebAuthn challenge store, and the unsealed agent tokens live. SEV-SNP closes this gap.

The audit log + WebAuthn + PQ crypto protect against:

- Network adversaries ✅
- Post-hoc quantum adversaries ✅
- The Vultr hypervisor ❌ (without SEV-SNP)

SEV-SNP protects against the Vultr hypervisor.

---

## Provisioning a SEV-SNP Vultr Box

> **W7-T1 (provision SEV-SNP Vultr box) cannot be completed from the development environment.** This section documents the procedure for ops.

1. Log in to the Vultr dashboard
2. Create a new server
3. Select a **High Frequency** plan
4. Select a region that supports SEV-SNP (check Vultr docs for current availability)
5. Select **Rocky Linux 9** (or 10) as the OS
6. In server settings, enable **AMD SEV-SNP** (if available for the selected plan/region)
7. Boot the server

### Verify SEV-SNP is available

```bash
# The guest-side device node (this is what the sev crate opens):
ls -la /dev/sev-guest
# Should show: crw------- 1 root root 10, 124 ... /dev/sev-guest

# The host-side device node (also present, but the guest does not use it directly):
ls -la /dev/sev

# Check CPU support
grep -w sev /proc/cpuinfo
# Should show: sev sev_es
```

If `/dev/sev-guest` is missing but `grep sev /proc/cpuinfo` shows the flags, load the kernel module:

```bash
modprobe sev-guest
```

---

## Attestation Flow

### 1. Gateway boots inside SEV-SNP guest

When the gateway starts, it:

1. Detects `/dev/sev-guest` via `verify_sev_snp_available()`
2. Generates an attestation report via `sev::firmware::guest::Firmware::get_report(None, None, Some(1))`
3. The report (`sev::firmware::guest::AttestationReport`, a `#[repr(C)]` struct) contains:
   - **`measurement`** (48 bytes) — SHA-384 digest of the binary + kernel + initrd + launch digest
   - **`report_data`** (64 bytes) — guest-provided opaque data
   - **`launch_tcb`** — TCB version at launch time
   - **`signature`** — ECDSA-SHA384 signature by the AMD VCEK (Versioned Chip Endorsement Key)
4. Serializes the report with `bincode`, base64-encodes it, hashes it with SHA-256, and returns the wrapper struct.

### 2. Gateway exposes attestation endpoint

```
GET /attestation
```

Returns:
```json
{
  "report": "base64:bincode-serialized-AttestationReport",
  "report_hash": "sha256-hex-of-report",
  "measurement": "sha384:<96 hex chars>",
  "sev_snp_active": true,
  "hardened_mode": true,
  "generated_at": "2026-07-29T14:23:01Z"
}
```

On the dev box (no `/dev/sev-guest`), the response is:
```json
{
  "report": "c3R1Yi1hdHRlc3RhdGlvbi1yZXBvcnQ=",
  "report_hash": "<sha256 of the above>",
  "measurement": "n/a",
  "sev_snp_active": false,
  "hardened_mode": false,
  "generated_at": "2026-07-29T14:23:01Z"
}
```

### 3. Phone verifies attestation before enrollment

When the tenant opens the enrollment URL:

1. The browser fetches `/attestation`
2. The measurement is displayed prominently
3. The tenant compares it with the published measurement in `docs/MEASUREMENTS/v1.0.txt`
4. If they match → proceed with enrollment
5. If they don't match → **STOP** — the gateway may be compromised

### 4. All subsequent approvals include the measurement

Every WebAuthn challenge includes the SEV-SNP measurement hash via `generate_challenge_with_sev_snp()`. If the measurement changes (due to a binary upgrade or compromise), the challenge also changes, so previously-issued assertions cannot be replayed.

The relevant code path (in `crypto/webauthn.rs`):

```rust,ignore
let measurement_hash = tee::generate_attestation_report()
    .ok()
    .map(|r| sev_snp_measurement_hash(&r));

let challenge = generate_challenge_with_sev_snp(
    cmd_hash, request_id, scope_hash,
    measurement_hash.as_deref(),
);
```

---

## Key Sealing

The audit signing keys (Ed25519 + ML-DSA-65) and push encryption keys (X25519 + ML-KEM-768) are **sealed to the launch measurement**.

### What this means

- Keys are encrypted with a key derived from the SEV-SNP launch measurement
- The keys can only be unsealed when the gateway is running with the exact same binary + kernel + initrd
- If the binary is modified (e.g., by an attacker), the measurement changes, and the keys cannot be unsealed

### Wire format

Sealed keys are stored as:

```text
[12-byte AES-GCM nonce] [ciphertext + 16-byte GCM tag]
```

The 12-byte nonce is randomly generated per call (CSPRNG). The AES-256-GCM authentication tag fails to verify if the key is wrong (different measurement) or the ciphertext was tampered with.

### Key-derivation paths

| Path | Key source | Used when |
|---|---|---|
| Real SEV-SNP | `sev::firmware::guest::Firmware::get_derived_key()` with `GuestFieldSelect(1 << 3)` (measurement mixed in) | Running inside an SEV-SNP guest |
| Dev fallback | HKDF-SHA256 from the measurement string | Dev box (no `/dev/sev-guest`) |
| No-SEV stub | (no encryption — pass-through) | `--features no-sev-snp` build |

### Key rotation ceremony

When upgrading the gateway:

```bash
stronghold upgrade
```

1. The old keys are loaded (using the old measurement)
2. A `key_rotation` audit entry is signed with the old keys
3. New keys are generated and sealed to the new measurement
4. All subsequent audit entries are signed with the new keys
5. Old keys are retained read-only for verifying historical entries

### Lost keys

If the keys are lost (e.g., the box is destroyed without backup):

- Historical audit entries can still be verified (if you have the old public keys)
- New entries cannot be signed with the old keys
- A new keypair is generated and a `key_regeneration` entry is recorded

---

## Verification

### Verify the attestation

```bash
# Fetch the attestation report
curl https://gateway:8443/attestation | jq

# Compare the measurement with the published value
curl https://gateway:8443/attestation | jq -r .measurement
# Should match: docs/MEASUREMENTS/v1.0.txt
```

### Verify the audit log includes SEV-SNP reports

```bash
stronghold audit verify --tenant <id>
```

This checks:

1. Hash chain is unbroken
2. Every Ed25519 signature verifies
3. Every ML-DSA-65 signature verifies (deferred to v1.1 — see `docs/CRYPTO.md`)
4. SEV-SNP attestation report hashes are present (when gateway was in TEE mode)
5. Attestation report hashes match the gateway's current measurement

---

## Performance Impact

| Metric | Impact |
|---|---|
| CPU overhead | ~5-10% (memory encryption) |
| Cold boot overhead | ~30 seconds (SEV-SNP launch + attestation) |
| Memory overhead | ~5% (integrity metadata) |
| Cost | SEV-SNP Vultr plans are ~$20-40/month extra |

---

## Development Without SEV-SNP

For development environments without SEV-SNP hardware:

```bash
# Build without SEV-SNP support (stubs return sev_snp_active=false)
cargo build --release --no-default-features --features no-sev-snp

# OR: build with both sev-snp (default) AND no-sev-snp feature flags
# (the sev-snp code is compiled but falls back to a stub at runtime
# because /dev/sev-guest is absent)
cargo build --release --features no-sev-snp
```

When running without SEV-SNP:

- `sev_snp_active: false` in the `/attestation` response
- Audit entries lack `sev_snp_report` and `sev_snp_report_hash`
- `stronghold audit verify` warns (but does not fail)
- The gateway runs normally but without TEE protection

**Do not use `--features no-sev-snp` in production.**

### Running the unit tests on the dev box

The full test suite passes on the dev box (no SEV-SNP hardware required):

```bash
cargo test --workspace --features no-sev-snp
```

The `tee/sealing.rs` module contains the HKDF + AES-256-GCM key-sealing primitives and is fully tested without any hardware. The `tee/sev_snp.rs` tests exercise the dev-fallback path (no `/dev/sev-guest`) and verify:

- Attestation report field structure and JSON serialization
- `sev_snp_active: false` on the dev box
- `seal_keys` → `unseal_keys` round-trip
- Sealed blob format (12-byte nonce prefix + ciphertext + GCM tag)
- Non-deterministic sealing (random nonce per call)
- Short-input rejection on unseal

---

## Troubleshooting

### `/dev/sev-guest` not found

```bash
# Check CPU support
grep -w sev /proc/cpuinfo

# Check if the kernel module is loaded
lsmod | grep sev

# Load the module
modprobe sev-guest

# If still not found, the Vultr plan does not support SEV-SNP
# Either upgrade to a SEV-SNP plan or run with --features no-sev-snp
```

### `/dev/sev` exists but `/dev/sev-guest` does not

`/dev/sev` is the host-side device node (used by the hypervisor). `/dev/sev-guest` is the guest-side device node (used by code running inside the SEV-SNP guest). The Stronghold gateway opens `/dev/sev-guest` because it runs as a guest, not as a host.

If you see `/dev/sev` but not `/dev/sev-guest`, you are probably on the hypervisor host, not inside an SEV-SNP guest.

### Attestation report generation fails

```bash
# Check permissions
ls -la /dev/sev-guest

# The gateway needs read access to /dev/sev-guest.
# Ensure the systemd unit runs as root, or add the gateway user
# to a group that has read access (distribution-dependent).
```

### Measurement mismatch after upgrade

This is expected. The measurement changes when the binary is upgraded.

1. Verify the new measurement matches `docs/MEASUREMENTS/v1.1.txt` (for the new version)
2. Run `stronghold keys rotate-audit` to seal keys to the new measurement
3. Re-enroll phones (the old enrollment is bound to the old measurement)

---

## Measurement Registry

Published measurements are stored in `docs/MEASUREMENTS/`:

```
docs/MEASUREMENTS/
├── v1.0.txt    # Stronghold v1.0.0  (placeholder until built on SEV-SNP hardware)
├── v1.1.txt    # Stronghold v1.1.0
└── v1.2.txt    # Stronghold v1.2.0
```

Each file is signed with the Stronghold release GPG key. Verify the signature before trusting the measurement:

```bash
gpg --verify docs/MEASUREMENTS/v1.0.txt.sig docs/MEASUREMENTS/v1.0.txt
```

The `v1.0.txt` file currently contains an all-zero placeholder. The actual measurement will be generated when the gateway binary is first built and run inside an SEV-SNP guest (blocked on W7-T1 — provisioning a Vultr SEV-SNP box).

---

## References

- [`sev` crate documentation](https://docs.rs/sev/4.0.0/sev/) — the Linux ioctl wrapper used by this crate
- [AMD SEV-SNP Firmware ABI Specification](https://www.amd.com/content/dam/amd/en/documents/epyc-technical-docs/specifications/56860.pdf) — the firmware ABI that the `sev` crate wraps
- ADR-0005: SEV-SNP in v1 (`docs/adr/0005-sev-snp-in-v1.md`)
- ADR-0006: WebAuthn not PQC (`docs/adr/0006-webauthn-not-pqc.md`)
- `docs/THREAT_MODEL.md` — threat model and SEV-SNP's role in it
