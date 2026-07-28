# Changelog

All notable changes to Stronghold will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial project scaffold
- Cargo workspace with `gateway` and `cli` crates
- Full architecture specification (18 sections)
- 10 Architecture Decision Records (ADRs)
- Image DSL specification (`image.toml` format)
- Agent protocol specification (ORDER / RESUME / RELEASE / EXTEND)
- Threat model document
- SEV-SNP attestation design
- Post-quantum cryptography stack (ML-KEM-768, ML-DSA-65, Ed25519, X25519)
- 8-image catalog (rocky-base + 7 derived images)
- Browser-only phone enrollment page (no custom app)
- Bootstrap scripts for control plane and workers
- systemd unit files

### Notes
- This is a **scaffold release**. Rust code compiles but functions are stubbed.
- The architecture, protocol, and crypto stack are fully specified.
- Implementation is the next phase.

## [0.1.0] - 2026-07-29

### Added
- Initial scaffold of the Stronghold project.
