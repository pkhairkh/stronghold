# Stronghold Fuzzing

This directory contains `cargo-fuzz` harnesses for security-critical parsers
and verifiers. Fuzzing runs the target with random inputs to find panics,
hangs, or logic bugs.

## Targets

| Target | What it fuzzes | Panic-free guarantee |
|---|---|---|
| `image_toml_parse` | `images::dsl::parse()` — image.toml parser | Parser must never panic on invalid TOML |
| `audit_verify_chain` | `AuditKeys::sign()` + `verify()` — sign/verify with random data + tamper | Sign/verify must never panic; tampered messages must always fail |
| `webauthn_assertion_decode` | `webauthn::parse_authenticator_data()` + `parse_and_validate_client_data()` | Parsers must never panic on malformed base64/JSON/binary |
| `hybrid_kem_encapsulate` | `hybrid_kem::encapsulate()` with random wrong-size keys | encapsulate must reject wrong sizes without panicking |

## Prerequisites

```bash
# Install cargo-fuzz (requires nightly Rust for libFuzzer)
cargo install cargo-fuzz
rustup toolchain install nightly
```

## Running

```bash
# Run each target for 1M iterations (or until a crash is found)
cd fuzz

cargo +nightly fuzz run image_toml_parse -- -max_total_time=300
cargo +nightly fuzz run audit_verify_chain -- -max_total_time=300
cargo +nightly fuzz run webauthn_assertion_decode -- -max_total_time=300
cargo +nightly fuzz run hybrid_kem_encapsulate -- -max_total_time=300

# Run a specific target with more iterations
cargo +nightly fuzz run image_toml_parse -- -max_total_time=3600 -jobs=4
```

## Crash Triage

If a crash is found:

1. The crash input is saved to `fuzz/artifacts/<target>/<sha1>`
2. Replay it: `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<sha1>`
3. Debug with: `cargo +nightly fuzz run <target> -- -minimize_crash=1 fuzz/artifacts/<target>/<sha1>`
4. Add a regression test in the corresponding Rust test module
5. Fix the bug
6. Re-run the fuzz target to confirm the fix

## CI Integration (TODO W11-T13)

The fuzz targets should run on CI for a fixed duration (e.g., 5 minutes per
target) on every PR. Any crash blocks the merge.
