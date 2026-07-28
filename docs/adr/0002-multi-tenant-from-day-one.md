# ADR 0002: Multi-tenant data model from day one

## Status

Accepted

## Context

Stronghold could be:
1. A personal tool (single tenant, you alone use it)
2. A team tool (a few tenants, your colleagues)
3. A community-hosted service (many tenants, anyone can sign up)

Even if it starts as a personal tool, the question is whether to design the data model as multi-tenant from the start or add multi-tenancy later.

## Decision

Design the data model as **multi-tenant from day one**. Every database table has `tenant_id` as the first column. Every query is tenant-scoped.

## Alternatives Considered

### Start single-tenant, add multi-tenancy later
- **Pros:** Less code to write upfront, faster time-to-first-commit
- **Cons:** Retrofitting multi-tenancy is a rewrite, not a refactor. By the time you need it, the gateway's data model assumes one human, the audit log has no tenant field, the image cache is global, and you're refactoring under load.

### Use a separate database per tenant
- **Pros:** Strongest isolation
- **Cons:** Connection pool exhaustion with many tenants, harder to do cross-tenant queries (e.g., admin dashboards), more complex backup/restore

## Consequences

### Positive
- No rewrite needed when adding the second tenant
- Clean separation of concerns
- Per-tenant audit logs, quotas, and credentials
- Natural path to community-hosted deployment

### Negative
- ~15% more code (every query includes `tenant_id`)
- Every function signature carries `tenant_id`
- Slightly more complex testing (need to set up tenant context)

### Neutral
- SQLite handles this well (just a column + index)
- The overhead is constant — it doesn't grow with the number of tenants

## Implementation

Every table starts with `tenant_id`:

```sql
CREATE TABLE audit_entries (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id   TEXT NOT NULL,          -- first column
    ts          TEXT NOT NULL,
    ...
);
CREATE INDEX idx_audit_tenant ON audit_entries(tenant_id, seq);
```

Every query is scoped:

```rust
// Good
conn.query_row(
    "SELECT * FROM machines WHERE tenant_id = ?1 AND id = ?2",
    params![tenant_id, machine_id],
)?;

// Bad (would allow cross-tenant access)
conn.query_row(
    "SELECT * FROM machines WHERE id = ?1",
    params![machine_id],
)?;
```
