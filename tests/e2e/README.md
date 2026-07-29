# Stronghold E2E Test Harness

This directory contains end-to-end integration tests for Stronghold.

## Test Categories

### Unit-level Integration Tests (Rust)
Located in `gateway/src/` modules under `#[cfg(test)]`:
- Session lifecycle (create → approve → finalize → resume → release)
- Audit log (write → hash chain → verify → tamper detection)
- Crypto round-trips (sign/verify, encapsulate/decapsulate, encrypt/decrypt)
- Database (init → migrate → CRUD → quota enforcement)

### HTTP API Tests (Rust + axum)
Located in `tests/e2e/api_tests.rs`:
- Start gateway, send HTTP requests, verify responses
- Test all agent protocol endpoints (ORDER/RESUME/RELEASE/EXTEND)
- Test phone endpoints (enroll, decide, revoke)
- Test admin endpoints (tenant CRUD)

### Full E2E Tests (Python + pytest)
Located in `tests/e2e/python/`:
- Start gateway + ntfy on dev box
- Simulate agent via HTTP requests
- Simulate phone via Playwright browser automation
- Verify full lifecycle: ORDER → approve → PTY → RELEASE → audit verify

## Running Tests

```bash
# All Rust tests (unit + integration)
cargo test --workspace --features no-sev-snp

# Only integration tests
cargo test --workspace --features no-sev-snp --test '*'

# Python E2E tests (requires gateway running)
cd tests/e2e/python
pip install pytest paramiko playwright
pytest -v
```

## CI Integration

The CI pipeline (`.github/workflows/ci.yml`) runs:
1. `cargo build --workspace --all-features`
2. `cargo test --workspace`
3. `cargo clippy -- -D warnings`
4. `cargo fmt --check`
5. `cargo audit`
6. Coverage reporting via `cargo tarpaulin`
