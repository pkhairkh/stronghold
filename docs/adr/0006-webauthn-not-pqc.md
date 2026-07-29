# ADR 0006: WebAuthn stays classical (not PQC)

## Status

Accepted

## Context

Stronghold uses WebAuthn for phone-based session approval (Face ID / Touch ID / YubiKey). The question is whether to use post-quantum algorithms for WebAuthn, given that we use PQC everywhere else.

## Decision

Keep WebAuthn on **classical cryptography** (ES256, RS256, Ed25519). Do not attempt to use PQC for WebAuthn in v1.

## Alternatives Considered

### Use PQC WebAuthn
- **Not possible today.** The FIDO Alliance has a PQC authenticator spec in draft (new COSE algorithm IDs for ML-DSA), but no deployed phone authenticator supports it. Apple's Secure Enclave and Google's StrongBox do not support PQC signing.
- Even if we implemented it in software, it would not be hardware-backed — defeating the purpose of WebAuthn.

### Replace WebAuthn with a custom PQC signing scheme
- **Pros:** Could use ML-DSA immediately
- **Cons:** Loses all the benefits of WebAuthn:
  - No biometric verification (Face ID / Touch ID)
  - No secure enclave key storage
  - No phishing resistance
  - No platform integration
  - We'd be rolling our own crypto auth — a well-known anti-pattern

## Consequences

### Positive
- WebAuthn works today, on every modern phone, with biometrics
- No need to wait for FIDO Alliance / Apple / Google to ship PQC authenticators (~2027 expected)
- Full phishing resistance, hardware-backed key storage, biometric verification

### Negative
- The one layer where PQC is not deployed
- A quantum adversary breaking WebAuthn's classical crypto in 10 years could forge approvals

### Mitigation

WebAuthn's role in Stronghold is **short-lived session approval**:
- Session TTLs are hours (default 4h, max 8h)
- The WebAuthn signature only opens a session — it doesn't protect long-term data
- The audit log (dual-signed with Ed25519 + ML-DSA-65) still provides post-quantum non-repudiation
- A quantum adversary breaking WebAuthn in 10 years gets nothing useful — sessions long expired, audit log still proves what happened

**Accept the gap.** Revisit when Apple/Google ship PQC authenticators (~2027).

## Implementation

Standard WebAuthn with `userVerification: "required"`:

```javascript
const assertion = await navigator.credentials.get({
    publicKey: {
        challenge: challenge,
        userVerification: "required",  // forces Face ID / Touch ID / PIN
        timeout: 60000,
    }
});
```

The WebAuthn challenge includes a SHA-256 of `(session_id, scope_hash, ttl, sev_snp_measurement)`, binding the approval to a specific session and attested gateway state.

## References

- [FIDO Alliance PQC roadmap](https://fidoalliance.org/)
- [webauthn-rs](https://docs.rs/webauthn-rs)
- [WebAuthn specification](https://www.w3.org/TR/webauthn/)
