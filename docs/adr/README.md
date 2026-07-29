# Architecture Decision Records (ADRs)

This directory contains Architecture Decision Records for Stronghold. ADRs capture the "why" behind each design choice — not just what was decided, but the context, alternatives considered, and consequences.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-use-rust-axum.md) | Use Rust + axum for the gateway | Accepted |
| [0002](0002-multi-tenant-from-day-one.md) | Multi-tenant data model from day one | Accepted |
| [0003](0003-k3s-worker-plane.md) | Use k3s as the worker plane | Accepted |
| [0004](0004-post-quantum-hybrid-everywhere.md) | Post-quantum hybrid cryptography everywhere | Accepted |
| [0005](0005-sev-snp-in-v1.md) | SEV-SNP in v1, not deferred | Accepted |
| [0006](0006-webauthn-not-pqc.md) | WebAuthn stays classical (not PQC) | Accepted |
| [0007](0007-ntfy-self-hosted-no-custom-app.md) | ntfy self-hosted, no custom phone app | Accepted |
| [0008](0008-image-toml-dsl.md) | image.toml DSL for image catalog | Accepted |
| [0009](0009-rocky-base-universal-root.md) | Rocky Linux as the universal base image | Accepted |
| [0010](0010-session-based-approval.md) | Session-based approval, not per-command | Accepted |

## Template

Use [0000-template.md](0000-template.md) as the starting point for new ADRs.

## Process

1. Copy `0000-template.md` to `NNNN-your-title.md`
2. Fill in the template
3. Submit a PR
4. Discuss and refine
5. Mark as Accepted, Deprecated, or Superseded
