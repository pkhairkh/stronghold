
---
Task ID: W9
Agent: general-purpose
Task: Wave 9 — CLI Implementation (W9-T1 through W9-T10)

Work Log:
- Rewrote cli/src/main.rs (372 → 1810 lines) to talk to the gateway over HTTP
  via reqwest. All subcommands now call the gateway API; stubs removed.
- Added `~/.stronghold.toml` config file support (Config struct + Settings
  resolver). Precedence: --url/--admin-token flags > STRONGHOLD_URL env >
  config file. New `--config`, `--insecure` flags.
- Added `GatewayClient` wrapper around `reqwest::Client` with centralized
  error handling: connect failures produce clear "Could not connect to
  gateway at <url>. Is the gateway running?" messages; HTTP errors surface
  status + body.
- Added `Completions { shell: Shell }` subcommand using `clap_complete`
  to generate bash/zsh/fish/elvish/powershell completion scripts.

Per-task DoD:
- W9-T1 (tenant create/list/get): POST /admin/tenant, GET /admin/tenant,
  GET /admin/tenant/:id. `create` prints setup_password + enrollment URL
  (built from gateway base URL if gateway returns only a path).
- W9-T2 (credentials enroll/list/revoke): `enroll` prints phone URL
  ({url}/setup or {url}/setup?tenant=<id> if --tenant given). `list`
  calls GET /admin/tenant/:id/credentials (table output). `revoke` calls
  DELETE /admin/credential/:id.
- W9-T3 (agent-token mint/list/revoke): POST /admin/agent-token (mint,
  prints token once), GET /admin/tenant/:id/agent-token (list table),
  POST /admin/agent-token/revoke (revoke by token string).
- W9-T4 (image build/list/push): `build` reads image.toml locally (validates
  TOML parse), POSTs raw toml + tag to /admin/image/build. `list` GETs
  /admin/image. `push` POSTs to /admin/image/push.
- W9-T5 (worker add/list): POST /admin/worker (add, prints worker info),
  GET /admin/worker (list table with SEV-SNP/CPU/status columns).
- W9-T6 (audit verify/export): tries gateway first (GET
  /admin/audit/:tenant/verify, GET /admin/audit/:tenant/export?from=&to=&format=);
  falls back to local SQLite DB if gateway unreachable. Local verifier
  re-derives SHA-256 hashes and checks the chain; local exporter emits
  JSON or text.
- W9-T7 (keys rotate-audit/rotate-push): POST /admin/keys/rotate-audit,
  POST /admin/keys/rotate-push. Prints confirmation + next steps.
- W9-T8 (init): creates data_dir/keys/audit/, initializes DB from
  gateway/src/db/schema.sql, generates Ed25519 audit keypair (mode 0600).
  Push keys + ML-DSA-65 left for gateway's `load_or_generate_keys` on
  first start (avoids pulling x25519-dalek/ml-kem into the CLI).
- W9-T9 (config file): Config struct deserialized from TOML; Settings
  resolver merges config + flags + env. Tested with unit tests for
  flag-overrides-config and config-only cases.
- W9-T10 (shell completion): `stronghold completions --shell bash|zsh|fish`
  via clap_complete::generate. Tested manually — bash completion script
  emits correctly.

Tests (13 new unit tests, all pass):
- Config parsing: full, partial, empty, missing file
- Settings resolution: flag overrides config, config-only case
- Local audit verify: empty DB (OK), tamper detection (hash mismatch caught)
- Local audit export: JSON format, text format, unsupported format errors
- write_secret creates files with mode 0600

Build verification:
- cargo build --workspace --features no-sev-snp → 0 errors, 0 warnings
- cargo clippy -p stronghold -- -D warnings → CLEAN
- cargo test --workspace --features no-sev-snp → 240 passed, 0 failed
  (13 new CLI tests + 227 pre-existing gateway tests)
- Manual smoke tests: --help, completions --shell bash, tenant create
  (clear error when no URL), init (creates DB + keys), credentials enroll
  (prints URL), audit verify (local fallback works)

Issues encountered:
- Initial uploads to dev box were silently overwritten by a parallel `git
  pull` (W6/W10 commits landing concurrently). Re-uploaded after pull.
- Three audit tests initially failed due to FOREIGN KEY constraint
  (audit_entries.tenant_id → tenants.id). Fixed by inserting a parent
  tenant row in each test fixture.
- Five clippy `print_literal` warnings on table-header println!s where
  the last column label was a bare string literal. Inlined the literal
  into the format string.

Files Changed:
- Cargo.toml                    (added clap_complete = "4.5" to workspace deps)
- cli/Cargo.toml                (added clap_complete + rand deps)
- cli/src/main.rs               (rewritten, 372 → 1810 lines)

Stage Summary:
- Wave 9 (CLI Implementation) COMPLETE
- All 10 tasks (W9-T1 through W9-T10) addressed
- CLI talks to gateway via reqwest with clear error messages on connect
  failure
- Config file (~/.stronghold.toml) + flag + env precedence works
- Shell completion generation for bash/zsh/fish
- Local fallback for audit verify/export when gateway unreachable
- init generates Ed25519 audit keypair locally
- 13 unit tests pass; clippy clean; full workspace builds + tests pass
- Gateway API endpoints assumed (e.g. /admin/tenant list, /admin/worker,
  /admin/keys/rotate-*) don't all exist yet in gateway/routes/mod.rs —
  will be wired up in Wave 11 (Integration & E2E). CLI is ready for them.
