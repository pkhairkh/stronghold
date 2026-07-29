//! Database migrations (placeholder for future schema changes).
//!
//! Currently the schema is created fresh from schema.sql. When the schema
//! changes in a future release, migrations will be added here.

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub fn run_migrations(_pool: &Pool<SqliteConnectionManager>) -> Result<()> {
    // No migrations yet — schema.sql creates everything from scratch.
    // Future migrations will be numbered and applied in order.
    Ok(())
}
