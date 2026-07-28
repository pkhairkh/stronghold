# ADR 0010: Session-based approval, not per-command

## Status

Accepted

## Context

Stronghold needs an approval model for AI agent sessions. The question is: what is the unit of approval?

Options:
1. **Per-command** — every command the agent runs requires phone approval
2. **Per-session** — one approval opens a TTL'd workspace; the agent has full PTY access
3. **Hybrid** — session-based by default, with quorum re-approval for destructive operations

## Decision

Use **session-based approval with quorum for destructive operations**.

- One phone tap opens a TTL'd workspace (default 4 hours)
- The agent has full PTY access during the session
- Destructive operations (`rm -rf`, `git push --force`, `DROP TABLE`) trigger quorum re-approval mid-session
- The tenant can revoke at any time (instant kill, <500ms)

## Alternatives Considered

### Per-command approval
- **Pros:** Maximum control — nothing happens without explicit approval
- **Cons:** Unusable for real work. A typical dev session involves hundreds of commands (`ls`, `cat`, `git status`, `cargo build`, `cargo test`, etc.). The tenant would stop reading approvals after the 12th one. Approval fatigue makes the system less secure, not more.

### Pure session-based (no mid-session re-approval)
- **Pros:** Simplest, lowest friction
- **Cons:** No protection against destructive operations within a session. If the agent runs `rm -rf /` mid-session, it executes before the tenant can react.

### Per-file approval
- **Pros:** Granular control
- **Cons:** Even worse than per-command — file access is more frequent than commands. Completely unusable.

## Consequences

### Positive
- Usable for real work — one tap, 4 hours of full agent access
- Destructive operations still trigger re-approval (quorum)
- Tenant can revoke instantly at any time
- Anomaly scanner pushes the phone for review (without blocking) on suspicious patterns
- Audit log records everything — forensic, not preventive, control

### Negative
- Within a session, the agent can run non-destructive but unwanted commands (e.g., exfiltrate via an allowed host)
- Mitigated by: network policy (default-deny egress), anomaly scanner (pushes on `curl`/`wget`), audit log (forensic trail)

### Neutral
- The "trust but verify" model: trust the agent for N hours, verify everything via audit log, cut it off if something looks wrong
- Quorum for destructive ops is the safety valve — two-phone approval for `rm -rf`-class operations

## Implementation

### Scopes

```toml
# per-tenant scopes.toml
[[scopes]]
name = "default"
shell = "full PTY"
require_credentials = 1
ttl_secs = 14400          # 4 hours

[[scopes]]
name = "destructive"
patterns = ["rm -rf", "git push --force", "DROP TABLE", "sudo rm"]
require_credentials = 2   # quorum: two phones must approve
ttl_secs = 1800           # 30 min window for the destructive op
```

### Flow

1. Agent calls `POST /agent/order` → phone buzzes → tenant taps Approve → Face ID → session starts
2. Agent has full PTY for 4 hours
3. If agent runs `rm -rf target/` → matches destructive pattern → command blocks → phone buzzes → both enrolled credentials must approve → command executes (or is denied)
4. If agent runs `curl evil.com` → anomaly scanner pushes phone for review → command executes (not blocked) → tenant can tap REVOKE if it looks wrong
5. At 4 hours, TTL expires → session ends, audit signed
6. Or tenant taps REVOKE at any time → session killed in <500ms

### Audit log

Every command (destructive or not) is logged with:
- Timestamp
- Command + SHA-256 hash
- Exit code
- stdout/stderr SHA-256 hashes
- Dual-signed (Ed25519 + ML-DSA-65)
- SEV-SNP attestation hash

The tenant can review the audit log after the fact and verify exactly what the agent did.

## References

- [Capability-based security](https://en.wikipedia.org/wiki/Capability-based_security)
- [Principle of least privilege](https://en.wikipedia.org/wiki/Principle_of_least_privilege)
