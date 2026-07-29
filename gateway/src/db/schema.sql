-- Stronghold database schema
-- Multi-tenant from day one: every table has tenant_id as the first column.

-- Tenants
CREATE TABLE IF NOT EXISTS tenants (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    setup_password  TEXT NOT NULL,
    setup_used      INTEGER DEFAULT 0,
    config          TEXT
);

-- WebAuthn credentials (per-tenant)
CREATE TABLE IF NOT EXISTS credentials (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    credential_id   TEXT NOT NULL,
    public_key      TEXT NOT NULL,
    aaguid          TEXT,
    transports      TEXT,
    name            TEXT,
    verified        INTEGER DEFAULT 0,
    created_at      TEXT NOT NULL,
    last_used_at    TEXT,
    revoked_at      TEXT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Agent tokens (per-tenant, TTL'd)
CREATE TABLE IF NOT EXISTS agent_tokens (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id       TEXT NOT NULL,
    token_hash      TEXT NOT NULL UNIQUE,
    scope           TEXT,
    created_at      TEXT NOT NULL,
    expires_at      TEXT,
    revoked_at      TEXT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Phone tokens (long-lived, revocable)
CREATE TABLE IF NOT EXISTS phone_tokens (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id       TEXT NOT NULL,
    token_hash      TEXT NOT NULL UNIQUE,
    created_at      TEXT NOT NULL,
    revoked_at      TEXT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Push encryption public keys (per-phone, uploaded at enrollment)
CREATE TABLE IF NOT EXISTS phone_push_keys (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id       TEXT NOT NULL,
    x25519_public   TEXT NOT NULL,
    mlkem_public    TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Pending sessions (awaiting phone approval)
CREATE TABLE IF NOT EXISTS pending_sessions (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    machine_id      TEXT,
    image           TEXT,
    ttl_secs        INTEGER,
    reason          TEXT,
    status          TEXT DEFAULT 'pending',  -- pending, approved, denied
    is_extend       INTEGER DEFAULT 0,
    created_at      TEXT NOT NULL,
    decided_at      TEXT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Active/recent machines
CREATE TABLE IF NOT EXISTS machines (
    id                  TEXT PRIMARY KEY,
    tenant_id           TEXT NOT NULL,
    image               TEXT NOT NULL,
    worker              TEXT NOT NULL,
    status              TEXT DEFAULT 'active',  -- active, released, revoked, expired, lost
    cpu                 INTEGER,
    memory_gb           INTEGER,
    worker_sev_snp      INTEGER DEFAULT 0,
    connect_token_hash  TEXT,                   -- SHA-256 of the connect_token issued at ORDER time
    created_at          TEXT NOT NULL,
    expires_at          TEXT NOT NULL,
    killed_at           TEXT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Per-tenant quotas
CREATE TABLE IF NOT EXISTS quotas (
    tenant_id                   TEXT PRIMARY KEY,
    max_concurrent_machines     INTEGER DEFAULT 5,
    max_cpu_per_machine         INTEGER DEFAULT 8,
    max_memory_gb_per_machine   INTEGER DEFAULT 16,
    max_disk_gb_per_machine     INTEGER DEFAULT 100,
    total_cpu_budget            INTEGER DEFAULT 16,
    total_memory_gb_budget      INTEGER DEFAULT 64,
    total_disk_gb_budget        INTEGER DEFAULT 500,
    require_sev_snp_workers     INTEGER DEFAULT 0,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Audit entries (hash-chained, dual-signed, SEV-SNP attested)
CREATE TABLE IF NOT EXISTS audit_entries (
    seq                 INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id           TEXT NOT NULL,
    ts                  TEXT NOT NULL,
    machine_id          TEXT,
    event               TEXT NOT NULL,
    payload             TEXT,
    prev_hash           TEXT NOT NULL,
    hash                TEXT NOT NULL,
    sig_ed25519         TEXT NOT NULL,
    sig_mldsa65         TEXT NOT NULL,
    sev_snp_report_hash TEXT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Workers
CREATE TABLE IF NOT EXISTS workers (
    id              TEXT PRIMARY KEY,
    host            TEXT NOT NULL UNIQUE,
    sev_snp         INTEGER DEFAULT 0,
    cpu_total       INTEGER,
    memory_gb_total INTEGER,
    disk_gb_total   INTEGER,
    status          TEXT DEFAULT 'active',
    created_at      TEXT NOT NULL,
    last_seen       TEXT
);

-- Tasks (structured work units with lifecycle)
CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    machine_id      TEXT,
    parent_task_id  TEXT,
    workflow_run_id TEXT,
    status          TEXT DEFAULT 'queued',   -- queued, scheduled, running, completed, failed, cancelled
    spec            TEXT NOT NULL,           -- JSON: {instruction, context, timeout_secs, image}
    result          TEXT,                    -- JSON: {exit_code, stdout, stderr, summary, artifacts}
    created_at      TEXT NOT NULL,
    started_at      TEXT,
    finished_at     TEXT,
    error           TEXT,
    retry_count     INTEGER DEFAULT 0,
    max_retries     INTEGER DEFAULT 3,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id),
    FOREIGN KEY (machine_id) REFERENCES machines(id)
);

-- Workflows (DAG definitions)
CREATE TABLE IF NOT EXISTS workflows (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    name            TEXT NOT NULL,
    dag             TEXT NOT NULL,           -- JSON DAG: {steps: [{id, task, depends_on, condition}]}
    status          TEXT DEFAULT 'draft',    -- draft, active, archived
    created_at      TEXT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Workflow runs (execution instances)
CREATE TABLE IF NOT EXISTS workflow_runs (
    id              TEXT PRIMARY KEY,
    workflow_id     TEXT NOT NULL,
    tenant_id       TEXT NOT NULL,
    status          TEXT DEFAULT 'pending',  -- pending, running, completed, failed, cancelled
    current_steps   TEXT,                    -- JSON array of step IDs currently running
    completed_steps TEXT,                    -- JSON array of completed step IDs
    started_at      TEXT,
    finished_at     TEXT,
    result          TEXT,                    -- JSON summary of all step results
    FOREIGN KEY (workflow_id) REFERENCES workflows(id),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Task outputs (artifact passing between tasks)
CREATE TABLE IF NOT EXISTS task_outputs (
    task_id         TEXT NOT NULL,
    key             TEXT NOT NULL,
    value           TEXT,
    artifact_path   TEXT,
    PRIMARY KEY (task_id, key),
    FOREIGN KEY (task_id) REFERENCES tasks(id)
);

-- Credential vault (encrypted secrets for agent use)
CREATE TABLE IF NOT EXISTS agent_credentials (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    name            TEXT NOT NULL,
    kind            TEXT NOT NULL,           -- ssh_key, api_token, env_var, file
    encrypted_value BLOB NOT NULL,
    nonce           BLOB NOT NULL,
    env_var         TEXT,                    -- e.g., "GITHUB_TOKEN" (for env injection)
    mount_path      TEXT,                    -- e.g., "/home/dev/.ssh/id_ed25519" (for file injection)
    created_at      TEXT NOT NULL,
    rotated_at      TEXT,
    UNIQUE(tenant_id, name),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id)
);

-- Agent messages (inter-agent communication)
CREATE TABLE IF NOT EXISTS agent_messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    from_machine    TEXT NOT NULL,
    to_machine      TEXT,
    channel         TEXT NOT NULL,
    body            TEXT NOT NULL,
    created_at      TEXT NOT NULL
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_audit_tenant ON audit_entries(tenant_id, seq);
CREATE INDEX IF NOT EXISTS idx_machines_tenant ON machines(tenant_id, status);
CREATE INDEX IF NOT EXISTS idx_pending_tenant ON pending_sessions(tenant_id, status);
CREATE INDEX IF NOT EXISTS idx_agent_tokens_hash ON agent_tokens(token_hash);
CREATE INDEX IF NOT EXISTS idx_phone_tokens_hash ON phone_tokens(token_hash);
CREATE INDEX IF NOT EXISTS idx_credentials_tenant ON credentials(tenant_id, revoked_at);
CREATE INDEX IF NOT EXISTS idx_tasks_tenant ON tasks(tenant_id, status);
CREATE INDEX IF NOT EXISTS idx_tasks_machine ON tasks(machine_id);
CREATE INDEX IF NOT EXISTS idx_workflow_runs_status ON workflow_runs(status);
CREATE INDEX IF NOT EXISTS idx_agent_credentials_tenant ON agent_credentials(tenant_id);
CREATE INDEX IF NOT EXISTS idx_agent_messages_channel ON agent_messages(channel, created_at);
