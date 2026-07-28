//! Tenant registry — create, get, list tenants.

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

pub struct Tenant {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub setup_password: String,
}

/// Create a new tenant.
pub fn create(db: &Pool<SqliteConnectionManager>, name: &str) -> Result<Tenant> {
    let id = format!("tenant_{}", ulid::Ulid::new());
    let created_at = chrono::Utc::now().to_rfc3339();
    let setup_password = generate_setup_password();

    let conn = db.get()?;
    conn.execute(
        "INSERT INTO tenants (id, name, created_at, setup_password) VALUES (?1, ?2, ?3, ?4)",
        params![id, name, created_at, setup_password],
    )?;

    tracing::info!(tenant_id = %id, name = name, "Tenant created");

    Ok(Tenant {
        id,
        name: name.to_string(),
        created_at,
        setup_password,
    })
}

/// Get a tenant by ID.
pub fn get(db: &Pool<SqliteConnectionManager>, id: &str) -> Result<Tenant> {
    let conn = db.get()?;
    let mut stmt =
        conn.prepare("SELECT id, name, created_at, setup_password FROM tenants WHERE id = ?1")?;

    let tenant = stmt.query_row(params![id], |row| {
        Ok(Tenant {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            setup_password: row.get(3)?,
        })
    })?;

    Ok(tenant)
}

/// List all tenants.
pub fn list(db: &Pool<SqliteConnectionManager>) -> Result<Vec<Tenant>> {
    let conn = db.get()?;
    let mut stmt = conn
        .prepare("SELECT id, name, created_at, setup_password FROM tenants ORDER BY created_at")?;

    let tenants = stmt
        .query_map([], |row| {
            Ok(Tenant {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                setup_password: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(tenants)
}

fn generate_setup_password() -> String {
    use rand::Rng;
    let chars: Vec<char> = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
        .chars()
        .collect();
    (0..32)
        .map(|_| chars[rand::thread_rng().gen_range(0..chars.len())])
        .collect()
}
