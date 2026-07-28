# Stronghold CLI

The `stronghold` command-line tool for managing tenants, credentials, images, and audit logs.

## Usage

```bash
# Tenant management
stronghold tenant create --name "alice"
stronghold tenant list
stronghold tenant get <id>

# Credential management
stronghold credentials enroll
stronghold credentials list
stronghold credentials revoke <id>

# Agent token management
stronghold agent-token mint --tenant <id> --scope default --ttl 86400
stronghold agent-token list --tenant <id>
stronghold agent-token revoke <token>

# Image management
stronghold image build <path/to/image.toml>
stronghold image list
stronghold image push <name>

# Worker management
stronghold worker add --host <hostname>
stronghold worker list

# Audit
stronghold audit verify --tenant <id>
stronghold audit export --tenant <id> --from 2026-07-01 --to 2026-07-31 --format json

# Key rotation
stronghold keys rotate-audit
stronghold keys rotate-push
```
