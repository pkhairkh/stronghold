# ADR 0001: Use Rust + axum for the gateway

## Status

Accepted

## Context

Stronghold's gateway is a security-critical control plane that:
- Handles post-quantum cryptographic operations (ML-KEM, ML-DSA, Ed25519, X25519)
- Manages multi-tenant state in SQLite
- Serves HTTP/WebSocket endpoints (agent protocol, phone approval, PTY proxy)
- Runs inside an SEV-SNP confidential VM
- Must be a single static binary for easy deployment

The language choice affects: memory safety, crypto library availability, binary size, deployment simplicity, and long-term maintainability.

## Decision

Use **Rust** with the **axum** web framework.

## Alternatives Considered

### Python + FastAPI
- **Pros:** Fast to write, rich ecosystem, familiar to many
- **Cons:** No memory safety guarantees, Python runtime required on the server, larger attack surface, GIL limits concurrency, slower crypto operations, not a single binary

### Go + net/http
- **Pros:** Memory-safe, single binary, good concurrency, simple deployment
- **Cons:** Weaker type system than Rust, no algebraic data types, error handling is verbose, crypto library ecosystem is less mature for PQ algorithms, no zero-cost abstractions

### Node.js + Fastify
- **Pros:** Familiar to web developers, good async model
- **Cons:** Not memory-safe, requires Node.js runtime, npm dependency hell, not a single binary, weaker crypto story for PQ

### Rust + actix-web
- **Pros:** Faster than axum in some benchmarks
- **Cons:** More complex, uses actors model (overkill for this use case), heavier dependency tree

## Consequences

### Positive
- Memory safety without garbage collection
- Single static binary (~20MB stripped)
- First-class support for post-quantum crypto crates (RustCrypto ecosystem: `ml-dsa`, `ml-kem`, `ed25519-dalek`, `x25519-dalek`)
- `rustls` supports X25519Kyber768 hybrid TLS (the only major TLS library with stable PQ support)
- `webauthn-rs` is a pure-Rust WebAuthn implementation
- Type-safe error handling with `thiserror` + `anyhow`
- Excellent async story with `tokio`

### Negative
- Steeper learning curve than Python/Go
- Longer compile times (~90s for release build)
- Smaller developer pool than Python/Go

### Neutral
- `axum` is relatively young but well-maintained by the tokio team
- The `sev` crate for SEV-SNP is Rust-native

## References

- [axum documentation](https://docs.rs/axum)
- [RustCrypto post-quantum crates](https://github.com/RustCrypto)
- [rustls PQ support](https://rustls.org/)
