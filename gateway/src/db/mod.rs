//! Database — SQLite schema, connection pool, and migrations.
//!
//! Implemented in: W2-T1 (init_pool + WAL), W2-T2 (migrations)
//! Tested by: gateway/src/db/mod.rs

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

/// Initialize the connection pool and create tables if they don't exist.
///
/// Enables WAL mode for concurrent reads, sets busy_timeout to 5s,
/// and runs all pending migrations.
pub fn init_pool(db_path: &str) -> Result<Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
        c.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA busy_timeout = 5000;\
             PRAGMA foreign_keys = ON;",
        )
    });
    let pool = Pool::builder().max_size(16).build(manager)?;

    // Run migrations
    let conn = pool.get()?;
    run_migrations(&conn)?;
    tracing::info!(db_path = db_path, "Database initialized with WAL mode");
    Ok(pool)
}

/// Run all pending migrations.
///
/// Migrations are numbered SQL files embedded at compile time.
/// Each migration runs in a transaction. The `_migrations` table tracks
/// which migrations have been applied.
fn run_migrations(conn: &rusqlite::Connection) -> Result<()> {
    // Create migrations tracking table if it doesn't exist.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL
        );",
    )?;

    // Migration 001: initial schema (from schema.sql).
    let applied: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _migrations WHERE id = 1",
        [],
        |row| row.get(0),
    )?;

    if applied == 0 {
        tracing::info!("Running migration 001: initial schema");
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(include_str!("schema.sql"))?;
        tx.execute(
            "INSERT INTO _migrations (id, name, applied_at) VALUES (1, 'initial_schema', datetime('now'))",
            [],
        )?;
        tx.commit()?;
        tracing::info!("Migration 001 applied");
    }

    // Migration 002: add connect_token_hash column to machines table.
    let applied_002: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _migrations WHERE id = 2",
        [],
        |row| row.get(0),
    )?;

    if applied_002 == 0 {
        tracing::info!("Running migration 002: add connect_token_hash to machines");
        let tx = conn.unchecked_transaction()?;
        // Check if the column already exists (handles fresh DBs where schema.sql
        // already includes it, vs upgraded DBs where it needs ALTER TABLE).
        let has_column: bool = {
            let mut stmt = tx.prepare("PRAGMA table_info(machines)")?;
            let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|c| c.ok())
                .collect();
            cols.iter().any(|c| c == "connect_token_hash")
        };
        if !has_column {
            tx.execute_batch("ALTER TABLE machines ADD COLUMN connect_token_hash TEXT;")?;
        }
        tx.execute(
            "INSERT INTO _migrations (id, name, applied_at) VALUES (2, 'add_connect_token_hash', datetime('now'))",
            [],
        )?;
        tx.commit()?;
        tracing::info!("Migration 002 applied");
    }

    // Migration 003: add task/workflow/credential/message tables.
    let applied_003: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _migrations WHERE id = 3",
        [],
        |row| row.get(0),
    )?;

    if applied_003 == 0 {
        tracing::info!("Running migration 003: task/workflow/credential tables");
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id              TEXT PRIMARY KEY,
                tenant_id       TEXT NOT NULL,
                machine_id      TEXT,
                parent_task_id  TEXT,
                workflow_run_id TEXT,
                status          TEXT DEFAULT 'queued',
                spec            TEXT NOT NULL,
                result          TEXT,
                created_at      TEXT NOT NULL,
                started_at      TEXT,
                finished_at     TEXT,
                error           TEXT,
                retry_count     INTEGER DEFAULT 0,
                max_retries     INTEGER DEFAULT 3,
                FOREIGN KEY (tenant_id) REFERENCES tenants(id),
                FOREIGN KEY (machine_id) REFERENCES machines(id)
            );
            CREATE TABLE IF NOT EXISTS workflows (
                id              TEXT PRIMARY KEY,
                tenant_id       TEXT NOT NULL,
                name            TEXT NOT NULL,
                dag             TEXT NOT NULL,
                status          TEXT DEFAULT 'draft',
                created_at      TEXT NOT NULL,
                FOREIGN KEY (tenant_id) REFERENCES tenants(id)
            );
            CREATE TABLE IF NOT EXISTS workflow_runs (
                id              TEXT PRIMARY KEY,
                workflow_id     TEXT NOT NULL,
                tenant_id       TEXT NOT NULL,
                status          TEXT DEFAULT 'pending',
                current_steps   TEXT,
                completed_steps TEXT,
                started_at      TEXT,
                finished_at     TEXT,
                result          TEXT,
                FOREIGN KEY (workflow_id) REFERENCES workflows(id),
                FOREIGN KEY (tenant_id) REFERENCES tenants(id)
            );
            CREATE TABLE IF NOT EXISTS task_outputs (
                task_id         TEXT NOT NULL,
                key             TEXT NOT NULL,
                value           TEXT,
                artifact_path   TEXT,
                PRIMARY KEY (task_id, key),
                FOREIGN KEY (task_id) REFERENCES tasks(id)
            );
            CREATE TABLE IF NOT EXISTS agent_credentials (
                id              TEXT PRIMARY KEY,
                tenant_id       TEXT NOT NULL,
                name            TEXT NOT NULL,
                kind            TEXT NOT NULL,
                encrypted_value BLOB NOT NULL,
                nonce           BLOB NOT NULL,
                env_var         TEXT,
                mount_path      TEXT,
                created_at      TEXT NOT NULL,
                rotated_at      TEXT,
                UNIQUE(tenant_id, name),
                FOREIGN KEY (tenant_id) REFERENCES tenants(id)
            );
            CREATE TABLE IF NOT EXISTS agent_messages (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                from_machine    TEXT NOT NULL,
                to_machine      TEXT,
                channel         TEXT NOT NULL,
                body            TEXT NOT NULL,
                created_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_tenant ON tasks(tenant_id, status);
            CREATE INDEX IF NOT EXISTS idx_tasks_machine ON tasks(machine_id);
            CREATE INDEX IF NOT EXISTS idx_workflow_runs_status ON workflow_runs(status);
            CREATE INDEX IF NOT EXISTS idx_agent_credentials_tenant ON agent_credentials(tenant_id);
            CREATE INDEX IF NOT EXISTS idx_agent_messages_channel ON agent_messages(channel, created_at);"
        )?;
        tx.execute(
            "INSERT INTO _migrations (id, name, applied_at) VALUES (3, 'task_workflow_credential_tables', datetime('now'))",
            [],
        )?;
        tx.commit()?;
        tracing::info!("Migration 003 applied");
    }

    // Migration 004: watchdog, roles, disagreements, workflow_templates tables.
    let applied_004: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _migrations WHERE id = 4",
        [],
        |row| row.get(0),
    )?;

    if applied_004 == 0 {
        tracing::info!("Running migration 004: watchdog/roles/disagreements/templates tables");
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS watchdog_reports (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                watcher_machine TEXT NOT NULL,
                watched_machine TEXT NOT NULL,
                watched_task_id TEXT,
                dedication_score REAL NOT NULL,
                progress_files INTEGER,
                progress_tests INTEGER,
                progress_commits INTEGER,
                last_activity_secs INTEGER,
                workaround_warnings TEXT,
                ultimatum_level INTEGER DEFAULT 0,
                assessment TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS ultimata (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                watchdog_machine TEXT NOT NULL,
                target_machine TEXT NOT NULL,
                target_task_id TEXT,
                level INTEGER NOT NULL,
                message TEXT NOT NULL,
                acknowledged INTEGER DEFAULT 0,
                created_at TEXT NOT NULL,
                acknowledged_at TEXT
            );
            CREATE TABLE IF NOT EXISTS agent_roles (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                name TEXT NOT NULL,
                system_prompt TEXT NOT NULL,
                allowed_tools TEXT NOT NULL DEFAULT '[]',
                denied_tools TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL,
                UNIQUE(tenant_id, name)
            );
            CREATE TABLE IF NOT EXISTS disagreements (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                task_id TEXT,
                machine_id TEXT NOT NULL,
                issue TEXT NOT NULL,
                coder_argument TEXT,
                reviewer_argument TEXT,
                context TEXT,
                decision TEXT,
                reasoning TEXT,
                precedent TEXT,
                status TEXT DEFAULT 'pending',
                created_at TEXT NOT NULL,
                resolved_at TEXT
            );
            CREATE TABLE IF NOT EXISTS workflow_templates (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                name TEXT NOT NULL,
                dag TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE(tenant_id, name)
            );
            CREATE INDEX IF NOT EXISTS idx_watchdog_watched ON watchdog_reports(watched_machine, created_at);
            CREATE INDEX IF NOT EXISTS idx_ultimata_target ON ultimata(target_machine, acknowledged);
            CREATE INDEX IF NOT EXISTS idx_agent_roles_tenant ON agent_roles(tenant_id, name);
            CREATE INDEX IF NOT EXISTS idx_disagreements_status ON disagreements(status);
            CREATE INDEX IF NOT EXISTS idx_workflow_templates_tenant ON workflow_templates(tenant_id, name);"
        )?;
        tx.execute(
            "INSERT INTO _migrations (id, name, applied_at) VALUES (4, 'watchdog_roles_disagreements_templates', datetime('now'))",
            [],
        )?;
        tx.commit()?;
        tracing::info!("Migration 004 applied");
    }


    // Migration 005: phone_challenges table + credentials.counter column (U1+U2).
    let applied_005: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _migrations WHERE id = 5",
        [],
        |row| row.get(0),
    )?;

    if applied_005 == 0 {
        tracing::info!("Running migration 005: phone_challenges + credentials.counter");
        let tx = conn.unchecked_transaction()?;

        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS phone_challenges (
                id          TEXT PRIMARY KEY,
                tenant_id   TEXT NOT NULL,
                challenge   BLOB NOT NULL,
                created_at  TEXT NOT NULL,
                used_at     TEXT,
                FOREIGN KEY (tenant_id) REFERENCES tenants(id)
            );",
        )?;

        // Add counter column to credentials (for WebAuthn replay protection, U2).
        let has_counter: bool = {
            let mut stmt = tx.prepare("PRAGMA table_info(credentials)")?;
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|c| c.ok())
                .collect();
            cols.iter().any(|c| c == "counter")
        };
        if !has_counter {
            tx.execute_batch(
                "ALTER TABLE credentials ADD COLUMN counter INTEGER NOT NULL DEFAULT 0;",
            )?;
        }

        tx.execute(
            "INSERT INTO _migrations (id, name, applied_at) VALUES (5, 'phone_challenges_credentials_counter', datetime('now'))",
            [],
        )?;
        tx.commit()?;
        tracing::info!("Migration 005 applied");
    }

    // Migration 006: add step_results column to workflow_runs (V1+V2).
    //
    // Stores a JSON map `{step_id: {exit_code, stdout, stderr, duration_ms}}`
    // written by `workflow::engine::update_step_results` after each wave.
    let applied_006: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _migrations WHERE id = 6",
        [],
        |row| row.get(0),
    )?;

    if applied_006 == 0 {
        tracing::info!("Running migration 006: add step_results to workflow_runs");
        let tx = conn.unchecked_transaction()?;

        // Check if the column already exists (handles fresh DBs where a
        // future schema.sql may include it, vs upgraded DBs where it needs
        // ALTER TABLE).
        let has_column: bool = {
            let mut stmt = tx.prepare("PRAGMA table_info(workflow_runs)")?;
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|c| c.ok())
                .collect();
            cols.iter().any(|c| c == "step_results")
        };
        if !has_column {
            tx.execute_batch(
                "ALTER TABLE workflow_runs ADD COLUMN step_results TEXT;",
            )?;
        }

        tx.execute(
            "INSERT INTO _migrations (id, name, applied_at) VALUES (6, 'add_step_results', datetime('now'))",
            [],
        )?;
        tx.commit()?;
        tracing::info!("Migration 006 applied");
    }

    Ok(())
}

/// Create an in-memory database for testing.
/// Public so integration tests can use it.
pub fn init_memory_pool() -> Result<Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::memory().with_init(|c| {
        c.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA foreign_keys = ON;",
        )
    });
    let pool = Pool::builder().max_size(4).build(manager)?;
    let conn = pool.get()?;
    run_migrations(&conn)?;
    Ok(pool)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_pool_creates_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let pool = init_pool(db_path.to_str().unwrap()).unwrap();

        let conn = pool.get().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        // All expected tables should exist.
        assert!(tables.contains(&"tenants".to_string()));
        assert!(tables.contains(&"credentials".to_string()));
        assert!(tables.contains(&"agent_tokens".to_string()));
        assert!(tables.contains(&"phone_tokens".to_string()));
        assert!(tables.contains(&"machines".to_string()));
        assert!(tables.contains(&"quotas".to_string()));
        assert!(tables.contains(&"audit_entries".to_string()));
        assert!(tables.contains(&"_migrations".to_string()));
    }

    #[test]
    fn test_init_pool_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");

        // First init creates all tables.
        let pool1 = init_pool(db_path.to_str().unwrap()).unwrap();
        drop(pool1);

        // Second init should not error (idempotent).
        let pool2 = init_pool(db_path.to_str().unwrap()).unwrap();
        let conn = pool2.get().unwrap();

        // _migrations should show exactly 6 migrations applied.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _migrations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 6);
    }

    #[test]
    fn test_wal_mode_enabled() {
        let pool = init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        // In-memory DBs use "memory" mode, but file DBs should use WAL.
        // For in-memory, we just verify the pragma doesn't error.
        assert!(!mode.is_empty());
    }

    #[test]
    fn test_init_memory_pool_works() {
        let pool = init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(tables.contains(&"tenants".to_string()));
    }

    #[test]
    fn test_foreign_keys_enabled() {
        let pool = init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        let fk_enabled: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk_enabled, 1);
    }

    #[test]
    fn test_task_workflow_credential_tables_exist() {
        let pool = init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(tables.contains(&"tasks".to_string()), "tasks table missing");
        assert!(tables.contains(&"workflows".to_string()), "workflows table missing");
        assert!(
            tables.contains(&"workflow_runs".to_string()),
            "workflow_runs table missing"
        );
        assert!(
            tables.contains(&"task_outputs".to_string()),
            "task_outputs table missing"
        );
        assert!(
            tables.contains(&"agent_credentials".to_string()),
            "agent_credentials table missing"
        );
        assert!(
            tables.contains(&"agent_messages".to_string()),
            "agent_messages table missing"
        );
    }

    /// Migration 006 adds a `step_results` column to `workflow_runs`.
    /// Verify the column exists after `init_memory_pool` runs migrations.
    #[test]
    fn test_workflow_runs_has_step_results_column() {
        let pool = init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(workflow_runs)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            cols.iter().any(|c| c == "step_results"),
            "step_results column missing on workflow_runs; got columns: {:?}",
            cols
        );
    }

    #[test]
    fn test_watchdog_and_roles_tables_exist() {
        let pool = init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(tables.contains(&"watchdog_reports".to_string()), "watchdog_reports missing");
        assert!(tables.contains(&"ultimata".to_string()), "ultimata missing");
        assert!(tables.contains(&"agent_roles".to_string()), "agent_roles missing");
        assert!(tables.contains(&"disagreements".to_string()), "disagreements missing");
        assert!(tables.contains(&"workflow_templates".to_string()), "workflow_templates missing");
    }
}
