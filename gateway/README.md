# Stronghold Gateway

The control plane binary. Single Rust binary, ~20MB stripped, runs inside an AMD SEV-SNP confidential VM.

## Building

```bash
# Default build (with SEV-SNP support)
cargo build --release

# Build without SEV-SNP (for dev environments)
cargo build --release --features no-sev-snp
```

## Running

```bash
# Production (SEV-SNP required)
./stronghold-gateway --config /etc/stronghold/config.toml

# Development (SEV-SNP optional)
./stronghold-gateway --dev --config config/dev.toml
```

## Configuration

See `config/example.toml` for all configuration options.

## Architecture

The gateway is structured as an axum web server with the following modules:

- `routes/` — HTTP/WebSocket endpoints (agent, phone, admin, PTY, attestation)
- `tenants/` — Multi-tenant registry, quotas, auth
- `sessions/` — Session lifecycle (mint, revoke, TTL, scopes)
- `machines/` — k3s scheduler, worker management, Vultr VPS escalation
- `images/` — Image DSL parser, Containerfile generator, registry client
- `crypto/` — Post-quantum hybrid KEM, signatures, TLS config, WebAuthn
- `tee/` — SEV-SNP attestation driver (behind `sev-snp` feature)
- `audit/` — Dual-signed audit log, hash-chained, verifiable offline
- `push/` — ntfy client, PQC end-to-end encryption
- `anomaly/` — PTY stream pattern matching
- `db/` — SQLite schema and migrations

See `src/main.rs` for the entry point and `src/lib.rs` for the module tree.
