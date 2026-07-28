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
///
/// The setup_password is generated as plaintext, hashed (SHA-256) for storage,
/// and returned as plaintext so the CLI can display it once. The hash is
/// what's stored in the database — the plaintext is never persisted.
pub fn create(db: &Pool<SqliteConnectionManager>, name: &str) -> Result<Tenant> {
    use sha2::{Digest, Sha256};

    let id = format!("tenant_{}", ulid::Ulid::new());
    let created_at = chrono::Utc::now().to_rfc3339();
    let setup_password = generate_setup_password();

    // Hash the password for storage (never store plaintext).
    let mut hasher = Sha256::new();
    hasher.update(setup_password.as_bytes());
    let setup_password_hash = hex::encode(hasher.finalize());

    let conn = db.get()?;
    conn.execute(
        "INSERT INTO tenants (id, name, created_at, setup_password, setup_used) VALUES (?1, ?2, ?3, ?4, 0)",
        params![id, name, created_at, setup_password_hash],
    )?;

    tracing::info!(tenant_id = %id, name = name, "Tenant created");

    Ok(Tenant {
        id,
        name: name.to_string(),
        created_at,
        setup_password, // Return plaintext for one-time display
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_memory_pool;

    #[test]
    fn test_create_tenant() {
        let pool = init_memory_pool().unwrap();
        let tenant = create(&pool, "alice").unwrap();
        assert!(tenant.id.starts_with("tenant_"));
        assert_eq!(tenant.name, "alice");
        assert_eq!(tenant.setup_password.len(), 32);
        assert!(!tenant.created_at.is_empty());
    }

    #[test]
    fn test_get_tenant() {
        let pool = init_memory_pool().unwrap();
        let created = create(&pool, "bob").unwrap();
        let fetched = get(&pool, &created.id).unwrap();
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.name, "bob");
        assert_eq!(fetched.setup_password, created.setup_password);
    }

    #[test]
    fn test_get_nonexistent_tenant_errors() {
        let pool = init_memory_pool().unwrap();
        let result = get(&pool, "tenant_does_not_exist");
        assert!(result.is_err());
    }

    #[test]
    fn test_list_tenants() {
        let pool = init_memory_pool().unwrap();
        create(&pool, "alice").unwrap();
        create(&pool, "bob").unwrap();
        create(&pool, "charlie").unwrap();
        let tenants = list(&pool).unwrap();
        assert_eq!(tenants.len(), 3);
    }

    #[test]
    fn test_setup_password_is_unique() {
        let pool = init_memory_pool().unwrap();
        let t1 = create(&pool, "alice").unwrap();
        let t2 = create(&pool, "bob").unwrap();
        assert_ne!(t1.setup_password, t2.setup_password);
    }

    #[test]
    fn test_setup_password_is_alphanumeric() {
        let pool = init_memory_pool().unwrap();
        let tenant = create(&pool, "alice").unwrap();
        for c in tenant.setup_password.chars() {
            assert!(c.is_ascii_alphanumeric());
        }
    }

    #[test]
    fn test_create_100_tenants_and_list() {
        let pool = init_memory_pool().unwrap();
        for i in 0..100 {
            create(&pool, &format!("tenant_{}", i)).unwrap();
        }
        let tenants = list(&pool).unwrap();
        assert_eq!(tenants.len(), 100);
    }
}

