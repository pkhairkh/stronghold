# Security Policy

## Supported Versions

| Version | Supported          |
|---------|--------------------|
| 0.1.x   | :white_check_mark: |

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

1. **Post-Quantum Transport**: TLS 1.3 + X25519Kyber768 hybrid
2. **Dual-Signed Audit**: Ed25519 + ML-DSA-65 on every log entry
3. **SEV-SNP Confidential Computing**: Gateway runs in attested TEE
4. **WebAuthn Session Approval**: Phishing-resistant, biometric-verified
5. **Quorum for Destructive Ops**: Multi-credential approval for high-risk commands
6. **No External Providers for Content**: ntfy is self-hosted; APNs/FCM are wake-up only
7. **Fail Closed**: Every failure mode denies rather than allows

## Security Considerations for Operators

If you are running Stronghold:

1. **Verify SEV-SNP Measurement**: Before enrolling credentials, verify the `SEV_SNP_MEASUREMENT` matches the published measurement in `docs/MEASUREMENTS/`.
2. **Rotate Keys Regularly**: Use `stronghold keys rotate-audit` periodically.
3. **Review Audit Logs**: Run `stronghold audit verify` regularly to detect tampering.
4. **Keep Backups**: Use `stronghold backup` to encrypt and store key material off-box.
5. **Harden SSH**: Disable password auth, use ed25519 keys only, install fail2ban.
6. **Network Isolation**: Use Tailscale for worker-to-control-plane communication.
