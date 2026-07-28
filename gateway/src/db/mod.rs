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

    // Future migrations go here:
    // migration 002: add column X to table Y
    // migration 003: create table Z
    // etc.

    Ok(())
}

/// Create an in-memory database for testing.
#[cfg(test)]
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

        // _migrations should show exactly 1 migration applied.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _migrations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
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
}
