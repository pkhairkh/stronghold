# SEV-SNP Attestation Guide

## Overview

Stronghold's gateway runs inside an AMD SEV-SNP (Secure Encrypted Virtualization - Secure Nested Paging) confidential VM. This provides:

- **Encrypted RAM** — The hypervisor sees ciphertext only
- **Integrity protection** — The hypervisor cannot modify memory without detection
- **Attestation** — The phone can verify the gateway runs on genuine SEV-SNP hardware
- **Launch measurement binding** — Audit signing keys are sealed to the launch measurement

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

1. Log in to the Vultr dashboard
2. Create a new server
3. Select a **High Frequency** plan
4. Select a region that supports SEV-SNP (check Vultr docs for current availability)
5. Select **Rocky Linux 9** as the OS
6. In server settings, enable **AMD SEV-SNP** (if available for the selected plan/region)
7. Boot the server

### Verify SEV-SNP is available

```bash
ls -la /dev/sev
# Should show: crw------- 1 root root 10, 124 ... /dev/sev

# Check CPU support
grep -w sev /proc/cpuinfo
# Should show: sev sev_es
```

---

## Attestation Flow

### 1. Gateway boots inside SEV-SNP guest

When the gateway starts, it:
1. Detects `/dev/sev`
2. Generates an attestation report via the `sev` crate
3. The report includes:
   - **Measurement** — hash of the binary + kernel + initrd
   - **Launch time**
   - **Platform info** — CPU, firmware
   - **Report signature** — signed by AMD-CEH (Cloud Enclave Hardware) key

### 2. Gateway exposes attestation endpoint

```
GET /attestation
```

Returns:
```json
{
  "report": "base64:...",
  "report_hash": "sha256:abc123...",
  "measurement": "sha256:def456...",
  "sev_snp_active": true,
  "hardened_mode": true,
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

Every WebAuthn challenge includes the SEV-SNP measurement hash. If the measurement changes (due to a binary upgrade or compromise), approvals fail.

---

## Key Sealing

The audit signing keys (Ed25519 + ML-DSA-65) and push encryption keys (X25519 + ML-KEM-768) are **sealed to the launch measurement**.

### What this means

- Keys are encrypted with a key derived from the SEV-SNP launch measurement
- The keys can only be unsealed when the gateway is running with the exact same binary + kernel + initrd
- If the binary is modified (e.g., by an attacker), the measurement changes, and the keys cannot be unsealed

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
3. Every ML-DSA-65 signature verifies
4. SEV-SNP attestation reports are present (when gateway was in TEE mode)
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
# Build without SEV-SNP support
cargo build --release --features no-sev-snp

# Run in dev mode (skips SEV-SNP check)
stronghold-gateway serve --dev
```

When running without SEV-SNP:
- Audit entries lack `sev_snp_report` and `sev_snp_report_hash`
- `stronghold audit verify` warns (but does not fail)
- The gateway runs normally but without TEE protection

**Do not use `--features no-sev-snp` in production.**

---

## Troubleshooting

### /dev/sev not found

```bash
# Check CPU support
grep -w sev /proc/cpuinfo

# Check if the kernel module is loaded
lsmod | grep sev

# Load the module
modprobe kvm_amd sev=1

# If still not found, the Vultr plan does not support SEV-SNP
# Either upgrade to a SEV-SNP plan or run with --dev
```

### Attestation report generation fails

```bash
# Check permissions
ls -la /dev/sev

# The gateway needs root access to /dev/sev
# Ensure the systemd unit runs as root
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
├── v1.0.txt    # Stronghold v1.0.0
├── v1.1.txt    # Stronghold v1.1.0
└── v1.2.txt    # Stronghold v1.2.0
```

Each file is signed with the Stronghold release GPG key. Verify the signature before trusting the measurement:

```bash
gpg --verify docs/MEASUREMENTS/v1.0.txt.sig docs/MEASUREMENTS/v1.0.txt
```
