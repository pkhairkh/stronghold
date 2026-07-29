//! O2: End-to-end integration test for Stronghold's task model.
//!
//! Drives the full task lifecycle on an in-memory SQLite database
//! (no real k3s / worker / exec — the exec step is mocked by directly
//! transitioning the row):
//!
//! 1. tenant + quota + agent token (real functions)
//! 2. task inserted as `queued`
//! 3. status read back == `queued`
//! 4. status transitioned to `running`
//! 5. result submitted → status `completed` with result JSON
//! 6. status read back == `completed`
//! 7. audit entry written and verified
//! 8. sub-task with `parent_task_id` chained to the first
//! 9. sub-task existence verified
//! 10. total wall-clock < 5 seconds
//!
//! Also covers:
//! - Credential vault: store → retrieve → decrypt → match
//! - Workflow: 2-step DAG stored and parsed back
//!
//! Run with:
//!     cargo test --workspace --features no-sev-snp --test e2e_task_test

use std::time::{Duration, Instant};

use rusqlite::params;
use stronghold_gateway::audit::log as audit_log;
use stronghold_gateway::crypto::hybrid_sig::AuditKeys;
use stronghold_gateway::crypto::vault;
use stronghold_gateway::db::init_memory_pool;
use stronghold_gateway::tenants::{auth, quotas, registry};

/// Upper bound for the whole E2E workload. The in-memory DB + signing path
/// should be orders of magnitude faster than this; the bound is generous to
/// avoid flakiness on slow CI runners.
const MAX_ELAPSED: Duration = Duration::from_secs(5);

/// End-to-end task lifecycle: tenant → quota → agent token → task queued →
/// running → completed → audit → sub-task → workflow → credential vault.
#[test]
fn e2e_task_lifecycle_full_flow() {
    let start = Instant::now();

    // ----------------------------------------------------------------
    // 1. Initialize the in-memory DB pool + generate audit keys.
    // ----------------------------------------------------------------
    let pool = init_memory_pool().expect("init_memory_pool must succeed");
    let audit_keys = AuditKeys::generate();

    // ----------------------------------------------------------------
    // 2. Create a tenant, set quota, mint an agent token.
    // ----------------------------------------------------------------
    let tenant = registry::create(&pool, "e2e-tenant").expect("registry::create must succeed");
    quotas::set(&pool, &tenant.id, 5, 8, 16).expect("quotas::set must succeed");
    let agent_token = auth::mint_agent_token(&pool, &tenant.id, "default", 3600)
        .expect("mint_agent_token must succeed");

    // Verify the token round-trips through `verify_agent_token` — proves the
    // tenant + token rows are wired up correctly.
    let verified_tenant = auth::verify_agent_token(&pool, &agent_token)
        .expect("verify_agent_token must succeed");
    assert_eq!(verified_tenant, tenant.id, "token must verify as the issuing tenant");

    // ----------------------------------------------------------------
    // 3. Insert a task directly into the tasks table with status='queued'.
    // ----------------------------------------------------------------
    let task_id = format!("task_{}", ulid::Ulid::new());
    let spec = serde_json::json!({
        "instruction": "echo hello from e2e",
        "image": "stronghold/rocky-base:latest",
        "ttl_secs": 600u64,
        "context": { "source": "e2e_task_test" },
    });
    let spec_str = spec.to_string();

    {
        let conn = pool.get().expect("pool.get (insert task) must succeed");
        conn.execute(
            "INSERT INTO tasks
             (id, tenant_id, machine_id, parent_task_id, workflow_run_id,
              status, spec, result, created_at, started_at, finished_at,
              error, retry_count, max_retries)
             VALUES (?1, ?2, NULL, NULL, NULL, 'queued', ?3, NULL,
                     datetime('now'), NULL, NULL, NULL, 0, 3)",
            params![task_id, tenant.id, spec_str],
        )
        .expect("INSERT into tasks must succeed");
    }

    // ----------------------------------------------------------------
    // 4. Read the task back and verify status == 'queued'.
    // ----------------------------------------------------------------
    {
        let conn = pool.get().expect("pool.get (verify queued) must succeed");
        let (status, fetched_spec): (String, String) = conn
            .query_row(
                "SELECT status, spec FROM tasks WHERE id = ?1 AND tenant_id = ?2",
                params![task_id, tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("queued task must be readable");
        assert_eq!(status, "queued", "task must be queued immediately after insert");
        let fetched_spec_json: serde_json::Value =
            serde_json::from_str(&fetched_spec).expect("spec must be valid JSON");
        assert_eq!(fetched_spec_json, spec, "spec must round-trip unchanged");
    }

    // ----------------------------------------------------------------
    // 5. Transition the task to 'running' (scheduler picks it up).
    // ----------------------------------------------------------------
    {
        let conn = pool.get().expect("pool.get (-> running) must succeed");
        let updated = conn.execute(
            "UPDATE tasks
             SET status = 'running', started_at = datetime('now')
             WHERE id = ?1 AND tenant_id = ?2 AND status = 'queued'",
            params![task_id, tenant.id],
        )
        .expect("UPDATE to running must succeed");
        assert_eq!(updated, 1, "exactly one row should transition queued -> running");
    }

    // ----------------------------------------------------------------
    // 6. Submit a result: status -> 'completed' with result JSON.
    // ----------------------------------------------------------------
    let result_json = serde_json::json!({
        "exit_code": 0,
        "stdout": "hello from e2e\n",
        "stderr": "",
        "summary": "echo succeeded",
        "artifacts": [],
    });
    let result_str = result_json.to_string();

    {
        let conn = pool.get().expect("pool.get (-> completed) must succeed");
        let updated = conn.execute(
            "UPDATE tasks
             SET status = 'completed', result = ?1, finished_at = datetime('now')
             WHERE id = ?2 AND tenant_id = ?3 AND status = 'running'",
            params![result_str, task_id, tenant.id],
        )
        .expect("UPDATE to completed must succeed");
        assert_eq!(
            updated, 1,
            "exactly one row should transition running -> completed"
        );
    }

    // ----------------------------------------------------------------
    // 7. Verify the task is now 'completed' and the result JSON matches.
    // ----------------------------------------------------------------
    {
        let conn = pool.get().expect("pool.get (verify completed) must succeed");
        let (status, fetched_result_opt): (String, Option<String>) = conn
            .query_row(
                "SELECT status, result FROM tasks WHERE id = ?1 AND tenant_id = ?2",
                params![task_id, tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("completed task must be readable");
        assert_eq!(status, "completed", "task must be completed after result submit");
        let fetched_result_str =
            fetched_result_opt.expect("result column must be non-null after completion");
        let fetched_result: serde_json::Value =
            serde_json::from_str(&fetched_result_str).expect("result must be valid JSON");
        assert_eq!(fetched_result, result_json, "result JSON must round-trip unchanged");
    }

    // ----------------------------------------------------------------
    // 8. Write an audit entry for the completion and verify it exists.
    // ----------------------------------------------------------------
    let audit_payload = serde_json::json!({
        "task_id": task_id,
        "exit_code": 0,
        "status": "completed",
        "summary": "echo succeeded",
    });
    audit_log::entry(
        &pool,
        &tenant.id,
        "", // machine_id: this task was never assigned a machine
        "task_completed",
        audit_payload.clone(),
        &audit_keys,
    )
    .expect("audit::log::entry must succeed");

    {
        let conn = pool.get().expect("pool.get (verify audit) must succeed");
        let (count, event, payload): (i64, String, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(event), MAX(payload)
                 FROM audit_entries WHERE tenant_id = ?1",
                params![tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("audit count query must succeed");
        assert_eq!(count, 1, "audit log should contain exactly one entry for tenant");
        assert_eq!(
            event, "task_completed",
            "audit event must be task_completed"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("audit payload must be JSON");
        assert_eq!(
            parsed["task_id"], task_id,
            "audit payload must reference the task we just completed"
        );
    }

    // ----------------------------------------------------------------
    // 9. Create a second task with parent_task_id set to the first
    //    (sub-task chaining) and verify it exists.
    // ----------------------------------------------------------------
    let sub_task_id = format!("task_{}", ulid::Ulid::new());
    let sub_spec = serde_json::json!({
        "instruction": "echo sub-task",
        "image": "stronghold/rocky-base:latest",
        "ttl_secs": 300u64,
        "context": { "parent": task_id },
    });
    let sub_spec_str = sub_spec.to_string();

    {
        let conn = pool.get().expect("pool.get (insert sub-task) must succeed");
        conn.execute(
            "INSERT INTO tasks
             (id, tenant_id, machine_id, parent_task_id, workflow_run_id,
              status, spec, result, created_at, started_at, finished_at,
              error, retry_count, max_retries)
             VALUES (?1, ?2, NULL, ?3, NULL, 'queued', ?4, NULL,
                     datetime('now'), NULL, NULL, NULL, 0, 3)",
            params![sub_task_id, tenant.id, task_id, sub_spec_str],
        )
        .expect("INSERT into tasks (sub-task) must succeed");
    }

    {
        let conn = pool.get().expect("pool.get (verify sub-task) must succeed");
        let (fetched_parent, fetched_status): (Option<String>, String) = conn
            .query_row(
                "SELECT parent_task_id, status FROM tasks WHERE id = ?1 AND tenant_id = ?2",
                params![sub_task_id, tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("sub-task must be readable");
        assert_eq!(
            fetched_parent.as_deref(),
            Some(task_id.as_str()),
            "sub-task parent_task_id must reference the parent task"
        );
        assert_eq!(fetched_status, "queued", "sub-task must start in queued state");

        // Also verify the parent task has exactly one child via a COUNT.
        let child_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1 AND tenant_id = ?2",
                params![task_id, tenant.id],
                |row| row.get(0),
            )
            .expect("COUNT of child tasks must succeed");
        assert_eq!(child_count, 1, "parent task should have exactly one child");
    }

    // ----------------------------------------------------------------
    // 10. Credential vault: encrypt → store → retrieve → decrypt → match.
    //     Uses the real vault primitives + the agent_credentials table.
    // ----------------------------------------------------------------
    let plaintext_secret = b"github_pat_e2e_super_secret_value_12345";
    let tenant_key = vault::derive_tenant_key(&tenant.id, &audit_keys);
    let (ciphertext, nonce) = vault::encrypt(plaintext_secret, &tenant_key)
        .expect("vault::encrypt must succeed");

    let cred_id = ulid::Ulid::new().to_string();
    let cred_created_at = chrono::Utc::now().to_rfc3339();
    {
        let conn = pool.get().expect("pool.get (insert credential) must succeed");
        conn.execute(
            "INSERT INTO agent_credentials
             (id, tenant_id, name, kind, encrypted_value, nonce,
              env_var, mount_path, created_at, rotated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, NULL)",
            params![
                cred_id,
                tenant.id,
                "github_token",
                "api_token",
                ciphertext,
                nonce,
                "GITHUB_TOKEN",
                cred_created_at,
            ],
        )
        .expect("INSERT into agent_credentials must succeed");
    }

    {
        let conn = pool.get().expect("pool.get (verify credential) must succeed");
        let (fetched_ct, fetched_nonce): (Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT encrypted_value, nonce FROM agent_credentials
                 WHERE id = ?1 AND tenant_id = ?2",
                params![cred_id, tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("credential row must be readable");

        let decrypted = vault::decrypt(&fetched_ct, &fetched_nonce, &tenant_key)
            .expect("vault::decrypt must succeed");
        assert_eq!(
            decrypted.as_slice(),
            plaintext_secret.as_slice(),
            "decrypted credential must match the original plaintext"
        );

        // Defense-in-depth: the stored ciphertext must NOT contain the plaintext
        // as a substring (confirms we actually encrypted, not just copied bytes).
        assert!(
            !fetched_ct
                .windows(plaintext_secret.len())
                .any(|w| w == plaintext_secret),
            "ciphertext must not leak the plaintext"
        );
    }

    // ----------------------------------------------------------------
    // 11. Workflow: create a workflow with 2 steps, verify the DAG is
    //     stored correctly.
    // ----------------------------------------------------------------
    let workflow_id = format!("wf_{}", ulid::Ulid::new());
    let dag = serde_json::json!({
        "steps": [
            {
                "id": "build",
                "task": "cargo build --release",
                "image": "stronghold/rust-nightly:latest",
                "depends_on": [],
                "condition": null,
                "max_retries": 3
            },
            {
                "id": "test",
                "task": "cargo test",
                "image": "stronghold/rust-nightly:latest",
                "depends_on": ["build"],
                "condition": "build.result.exit_code == 0",
                "max_retries": 1
            }
        ]
    });
    let dag_str = dag.to_string();
    let wf_created_at = chrono::Utc::now().to_rfc3339();

    {
        let conn = pool.get().expect("pool.get (insert workflow) must succeed");
        conn.execute(
            "INSERT INTO workflows (id, tenant_id, name, dag, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'draft', ?5)",
            params![workflow_id, tenant.id, "build-and-test", dag_str, wf_created_at],
        )
        .expect("INSERT into workflows must succeed");
    }

    {
        let conn = pool.get().expect("pool.get (verify workflow) must succeed");
        let (fetched_name, fetched_status, fetched_dag): (String, String, String) = conn
            .query_row(
                "SELECT name, status, dag FROM workflows
                 WHERE id = ?1 AND tenant_id = ?2",
                params![workflow_id, tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("workflow row must be readable");
        assert_eq!(fetched_name, "build-and-test");
        assert_eq!(fetched_status, "draft");

        let parsed_dag: serde_json::Value =
            serde_json::from_str(&fetched_dag).expect("stored DAG must be valid JSON");
        let steps = parsed_dag
            .get("steps")
            .and_then(|s| s.as_array())
            .expect("DAG must have a steps array");
        assert_eq!(steps.len(), 2, "DAG must contain exactly 2 steps");

        assert_eq!(steps[0]["id"], "build", "first step id must be 'build'");
        assert_eq!(steps[1]["id"], "test", "second step id must be 'test'");
        let depends: Vec<String> = steps[1]["depends_on"]
            .as_array()
            .expect("depends_on must be an array")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            depends,
            vec!["build".to_string()],
            "step 'test' must depend on step 'build'"
        );
    }

    // ----------------------------------------------------------------
    // 12. Assert the whole E2E flow completed in under MAX_ELAPSED.
    // ----------------------------------------------------------------
    let elapsed = start.elapsed();
    assert!(
        elapsed < MAX_ELAPSED,
        "E2E task lifecycle took {:?}, expected < {:?}",
        elapsed,
        MAX_ELAPSED
    );

    eprintln!(
        "O2 e2e_task_lifecycle_full_flow: tenant + task + sub-task + audit + credential + workflow in {:?} (< {:?})",
        elapsed, MAX_ELAPSED
    );
}
