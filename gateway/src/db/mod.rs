//! Database — SQLite schema and connection pool management.

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

/// Initialize the connection pool and create tables if they don't exist.
pub fn init_pool(db_path: &str) -> Result<Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(db_path);
    let pool = Pool::builder().max_size(16).build(manager)?;

    // Create tables
    let conn = pool.get()?;
    conn.execute_batch(include_str!("schema.sql"))?;

    tracing::info!("Database initialized at {}", db_path);
    Ok(pool)
}
