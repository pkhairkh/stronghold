//! C4: Load test — 100 concurrent sessions.
//!
//! This test exercises the full session lifecycle at scale on a single
//! in-memory SQLite database:
//!
//! 1. Initialize an in-memory DB pool.
//! 2. Create 100 tenants.
//! 3. Set quotas for each tenant.
//! 4. Mint 100 agent tokens (one per tenant).
//! 5. Create 100 pending sessions (one per tenant).
//! 6. Approve all 100 sessions.
//! 7. Write 100 audit entries (one per approved session).
//!
//! The whole workload must finish in under 30 seconds, and the audit log
//! must contain exactly 100 entries at the end.
//!
//! Run with:
//!     cargo test --workspace --features no-sev-snp --test load_test

use std::time::{Duration, Instant};

use stronghold_gateway::audit::log as audit_log;
use stronghold_gateway::crypto::hybrid_sig::AuditKeys;
use stronghold_gateway::db::init_memory_pool;
use stronghold_gateway::routes::agent::{ComputeRequest, OrderRequest};
use stronghold_gateway::sessions::manager as session_manager;
use stronghold_gateway::tenants::{auth, quotas, registry};

/// Maximum acceptable wall-clock time for the full 100-session workload.
const MAX_ELAPSED: Duration = Duration::from_secs(30);

/// How many concurrent sessions to drive through the gateway in this test.
const N: usize = 100;

/// Load test: drive 100 tenants → quotas → agent tokens → pending sessions →
/// approvals → audit entries through the gateway on a single in-memory DB,
/// then assert the workload completes in under 30 seconds and that the audit
/// log contains exactly 100 entries.
#[test]
fn load_test_100_concurrent_sessions() {
    // ----------------------------------------------------------------
    // 1. Initialize the in-memory database pool.
    // ----------------------------------------------------------------
    let pool = init_memory_pool().expect("init_memory_pool must succeed");

    // A single AuditKeys keypair signs all 100 audit entries. In production
    // each tenant would have its own keys, but for a load test we want to
    // isolate the cost of the DB + signing path, not key generation.
    let audit_keys = AuditKeys::generate();

    let start = Instant::now();

    // ----------------------------------------------------------------
    // 2. Create 100 tenants.
    // 3. Set quotas for each.
    // 4. Mint one agent token per tenant.
    //
    // We do these three steps in a single loop so each tenant is fully
    // provisioned (registry row + quota row + agent token) before we move
    // on to the next. This mirrors how the CLI bootstraps a tenant.
    // ----------------------------------------------------------------
    let mut tenant_ids = Vec::with_capacity(N);
    let mut agent_tokens = Vec::with_capacity(N);

    for i in 0..N {
        let tenant = registry::create(&pool, &format!("load-test-tenant-{}", i))
            .expect("registry::create must succeed");

        quotas::set(&pool, &tenant.id, 5, 8, 16)
            .expect("quotas::set must succeed");

        let token = auth::mint_agent_token(&pool, &tenant.id, "default", 3600)
            .expect("mint_agent_token must succeed");

        tenant_ids.push(tenant.id);
        agent_tokens.push(token);
    }

    assert_eq!(tenant_ids.len(), N, "should have created {} tenants", N);
    assert_eq!(
        agent_tokens.len(),
        N,
        "should have minted {} agent tokens",
        N
    );

    // ----------------------------------------------------------------
    // 5. Create 100 pending sessions (one per tenant).
    //
    // All 100 sessions now exist concurrently in the `pending_sessions`
    // table — i.e. the system has 100 concurrent pending approval requests
    // at this point.
    // ----------------------------------------------------------------
    let order_req = OrderRequest {
        image: "stronghold/rocky-base:latest".to_string(),
        ttl_secs: 3600,
        reason: "C4 load test".to_string(),
        compute: ComputeRequest {
            cpu: Some(4),
            memory_gb: Some(8),
            dedicated: Some(false),
            gpu: Some(false),
        },
        ephemeral_volumes: vec!["~/work".to_string(), "~/.cache".to_string()],
    };

    let mut session_ids = Vec::with_capacity(N);
    for tenant_id in &tenant_ids {
        let session_id = session_manager::create_pending(&pool, tenant_id, &order_req)
            .expect("create_pending must succeed");
        session_ids.push(session_id);
    }
    assert_eq!(session_ids.len(), N, "should have {} pending sessions", N);

    // ----------------------------------------------------------------
    // 6. Approve all 100 sessions.
    //
    // After this step all 100 sessions transition from `pending` →
    // `approved` — simulating the tenant tapping "Approve" on their phone
    // for each of the 100 concurrent requests.
    // ----------------------------------------------------------------
    for session_id in &session_ids {
        session_manager::approve_session(&pool, session_id)
            .expect("approve_session must succeed");
    }

    // ----------------------------------------------------------------
    // 7. Write 100 audit entries (one per approved session).
    //
    // Each entry is dual-signed (Ed25519 + ML-DSA-65) and hash-chained to
    // the previous entry for that tenant. Since each tenant has exactly
    // one audit entry, every entry's `prev_hash` is the zero hash.
    // ----------------------------------------------------------------
    for (i, tenant_id) in tenant_ids.iter().enumerate() {
        let machine_id = format!("machine_load_test_{}", i);
        audit_log::entry(
            &pool,
            tenant_id,
            &machine_id,
            "session_started",
            serde_json::json!({
                "session_id": session_ids[i],
                "image": order_req.image,
                "ttl_secs": order_req.ttl_secs,
                "reason": order_req.reason,
                "load_test": true,
            }),
            &audit_keys,
        )
        .expect("audit::log::entry must succeed");
    }

    let elapsed = start.elapsed();

    // ----------------------------------------------------------------
    // 8. Assert the whole workload completed in under 30 seconds.
    // ----------------------------------------------------------------
    assert!(
        elapsed < MAX_ELAPSED,
        "load test took {:?}, expected < {:?}",
        elapsed,
        MAX_ELAPSED
    );

    // ----------------------------------------------------------------
    // 9. Assert the audit log has exactly 100 entries.
    //
    // The `audit_entries` table uses a global AUTOINCREMENT `seq`, so the
    // total row count across all tenants should be exactly `N`.
    // ----------------------------------------------------------------
    let conn = pool.get().expect("pool.get must succeed");
    let audit_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM audit_entries", [], |row| row.get(0))
        .expect("COUNT(*) on audit_entries must succeed");
    assert_eq!(
        audit_count, N as i64,
        "audit log should have exactly {} entries, got {}",
        N, audit_count
    );

    // Sanity-check the rest of the workload while we have the connection:
    // these aren't part of the strict DoD but they catch regressions where
    // the load test "passes" because one of the steps silently no-op'd.
    let tenant_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM tenants", [], |row| row.get(0))
        .expect("COUNT(*) on tenants must succeed");
    assert_eq!(tenant_count, N as i64, "tenant count mismatch");

    let quota_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM quotas", [], |row| row.get(0))
        .expect("COUNT(*) on quotas must succeed");
    assert_eq!(quota_count, N as i64, "quota count mismatch");

    let token_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_tokens", [], |row| row.get(0))
        .expect("COUNT(*) on agent_tokens must succeed");
    assert_eq!(token_count, N as i64, "agent token count mismatch");

    let pending_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM pending_sessions", [], |row| {
            row.get(0)
        })
        .expect("COUNT(*) on pending_sessions must succeed");
    assert_eq!(pending_count, N as i64, "pending session count mismatch");

    let approved_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_sessions WHERE status = 'approved'",
            [],
            |row| row.get(0),
        )
        .expect("COUNT(*) on approved pending_sessions must succeed");
    assert_eq!(
        approved_count,
        N as i64,
        "all {} sessions should be approved",
        N
    );

    eprintln!(
        "C4 load test: {} tenants + {} sessions + {} audit entries in {:?} (< {:?})",
        N, N, N, elapsed, MAX_ELAPSED
    );
}
