//! T1: End-to-end integration test for Stronghold's multi-agent workflow
//! system.
//!
//! Drives the full multi-agent lifecycle on an in-memory SQLite database
//! (no real k3s / worker / exec — task execution is simulated by directly
//! transitioning rows):
//!
//! 1. tenant + quota + agent token (real functions)
//! 2. credential vault: github-pat encrypted + stored + retrieved + matched
//! 3. workflow definition (4-step DAG mirroring `standard-cicd`) stored
//! 4. workflow run started (`workflow_runs` row, status = `running`)
//! 5. each step simulated: task queued → running → completed (with result JSON)
//! 6. workflow run finalised → status = `completed`
//! 7. all 4 tasks verified to exist with `status = 'completed'`
//! 8. reflexion submitted for the final task → retrievable via `task_outputs`
//! 9. audit entries written for every step → tamper-evident chain verified
//!
//! The DAG engine itself (`workflow::engine::execute`) is extensively unit
//! tested in `gateway/src/workflow/engine.rs` (topological ordering, condition
//! evaluation, dependency resolution, retry semantics). This test focuses on
//! the data-model integration across `tenants`, `tasks`, `workflows`,
//! `workflow_runs`, `task_outputs`, `agent_credentials`, and `audit_entries`.
//!
//! Run with:
//!     cargo test --workspace --features no-sev-snp --test multi_agent_test

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

/// The 4 step IDs in the workflow DAG, in execution order.
const STEP_IDS: &[&str] = &["plan", "implement", "test", "merge"];

/// Build a 4-step linear DAG mirroring `agent/templates/standard-cicd.json`.
///
/// Each step depends on the previous one and carries an `exit_code == 0`
/// condition so the engine (if it were driving the run) would advance down
/// the chain. Conditions reference `<prev_step>.result.exit_code`, which the
/// engine resolves against the per-step results map.
fn build_standard_cicd_dag() -> serde_json::Value {
    serde_json::json!({
        "steps": [
            {
                "id": "plan",
                "task": "Analyze the issue, read the codebase, create an implementation plan",
                "image": "stronghold/rust-nightly:latest",
                "ttl_secs": 1800u64,
                "depends_on": [],
                "role": "planner"
            },
            {
                "id": "implement",
                "task": "Implement the fix. Create branch, write code, run tests, push, create PR.",
                "image": "stronghold/rust-nightly:latest",
                "ttl_secs": 7200u64,
                "depends_on": ["plan"],
                "role": "coder",
                "condition": "plan.result.exit_code == 0"
            },
            {
                "id": "test",
                "task": "Run full test suite. Report results.",
                "image": "stronghold/rust-nightly:latest",
                "ttl_secs": 1800u64,
                "depends_on": ["implement"],
                "role": "tester",
                "condition": "implement.result.exit_code == 0"
            },
            {
                "id": "merge",
                "task": "Merge the approved PR. Run CI on main.",
                "image": "stronghold/rust-nightly:latest",
                "ttl_secs": 1800u64,
                "depends_on": ["test"],
                "role": "integrator",
                "condition": "test.result.exit_code == 0"
            }
        ]
    })
}

/// End-to-end multi-agent workflow: tenant → quota → token → credential →
/// workflow → run → 4 simulated steps → completed → reflexion.
#[test]
fn e2e_multi_agent_workflow_full_flow() {
    let start = Instant::now();

    // ----------------------------------------------------------------
    // 1. Initialize the in-memory DB pool + generate audit keys.
    // ----------------------------------------------------------------
    let pool = init_memory_pool().expect("init_memory_pool must succeed");
    let audit_keys = AuditKeys::generate();

    // ----------------------------------------------------------------
    // 2. Create a tenant, set quota, mint an agent token.
    // ----------------------------------------------------------------
    let tenant = registry::create(&pool, "multi-agent-tenant")
        .expect("registry::create must succeed");
    quotas::set(&pool, &tenant.id, 5, 8, 16).expect("quotas::set must succeed");
    let agent_token = auth::mint_agent_token(&pool, &tenant.id, "default", 3600)
        .expect("mint_agent_token must succeed");

    // Verify the token round-trips through `verify_agent_token`.
    let verified_tenant = auth::verify_agent_token(&pool, &agent_token)
        .expect("verify_agent_token must succeed");
    assert_eq!(
        verified_tenant, tenant.id,
        "token must verify as the issuing tenant"
    );

    // ----------------------------------------------------------------
    // 3. Credential vault: store a github-pat, then retrieve + decrypt.
    //    Uses the real vault primitives (AES-256-GCM + HKDF-256) and the
    //    `agent_credentials` table — same path the Coder agent uses at
    //    runtime to fetch its GitHub PAT.
    // ----------------------------------------------------------------
    let plaintext_secret = b"github_pat_11AABBCCDDEE_secret_token_value";
    let tenant_key = vault::derive_tenant_key(&tenant.id, &audit_keys);
    let (ciphertext, nonce) =
        vault::encrypt(plaintext_secret, &tenant_key).expect("vault::encrypt must succeed");

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
                "github-pat",
                "api_token",
                ciphertext,
                nonce,
                "GITHUB_TOKEN",
                cred_created_at,
            ],
        )
        .expect("INSERT into agent_credentials must succeed");
    }

    // Retrieve + decrypt + match. Defense-in-depth: the ciphertext must not
    // leak the plaintext.
    {
        let conn = pool.get().expect("pool.get (verify credential) must succeed");
        let (fetched_ct, fetched_nonce, fetched_env_var): (Vec<u8>, Vec<u8>, String) = conn
            .query_row(
                "SELECT encrypted_value, nonce, env_var FROM agent_credentials
                 WHERE id = ?1 AND tenant_id = ?2",
                params![cred_id, tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("credential row must be readable");

        let decrypted =
            vault::decrypt(&fetched_ct, &fetched_nonce, &tenant_key)
                .expect("vault::decrypt must succeed");
        assert_eq!(
            decrypted.as_slice(),
            plaintext_secret.as_slice(),
            "decrypted github-pat must match the original plaintext"
        );
        assert_eq!(
            fetched_env_var, "GITHUB_TOKEN",
            "credential env_var must round-trip"
        );
        assert!(
            !fetched_ct
                .windows(plaintext_secret.len())
                .any(|w| w == plaintext_secret),
            "github-pat ciphertext must not leak the plaintext"
        );
    }

    // ----------------------------------------------------------------
    // 4. Create the workflow definition (4-step linear DAG).
    // ----------------------------------------------------------------
    let workflow_id = format!("wf_{}", ulid::Ulid::new());
    let dag = build_standard_cicd_dag();
    let dag_str = dag.to_string();
    let wf_created_at = chrono::Utc::now().to_rfc3339();

    {
        let conn = pool.get().expect("pool.get (insert workflow) must succeed");
        conn.execute(
            "INSERT INTO workflows (id, tenant_id, name, dag, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'draft', ?5)",
            params![workflow_id, tenant.id, "standard-cicd", dag_str, wf_created_at],
        )
        .expect("INSERT into workflows must succeed");
    }

    // ----------------------------------------------------------------
    // 5. Start the workflow run. The engine (`workflow::engine::execute`)
    //    is async + spawns tokio tasks that poll the tasks table; in this
    //    synchronous integration test we drive the run manually by
    //    transitioning the row + creating the per-step tasks. The engine's
    //    DAG-walking logic is independently covered by ~30 unit tests in
    //    `gateway/src/workflow/engine.rs`.
    // ----------------------------------------------------------------
    let run_id = format!("wfr_{}", ulid::Ulid::new());
    {
        let conn = pool.get().expect("pool.get (insert run) must succeed");
        conn.execute(
            "INSERT INTO workflow_runs
             (id, workflow_id, tenant_id, status, current_steps, completed_steps,
              started_at)
             VALUES (?1, ?2, ?3, 'running', '[]', '[]', datetime('now'))",
            params![run_id, workflow_id, tenant.id],
        )
        .expect("INSERT into workflow_runs must succeed");
    }

    // ----------------------------------------------------------------
    // 6. Simulate each step: insert a queued task → set running → submit
    //    a successful result. Mirror the engine's `run_step_once` shape:
    //    the task spec is `{instruction, image, ttl_secs, context, role}`
    //    and the result is `{exit_code, stdout, stderr, summary, artifacts}`.
    // ----------------------------------------------------------------
    let dag_steps = dag
        .get("steps")
        .and_then(|s| s.as_array())
        .expect("DAG must have a steps array");

    let mut task_ids: Vec<String> = Vec::with_capacity(dag_steps.len());
    let mut completed_steps: Vec<String> = Vec::with_capacity(dag_steps.len());

    for (idx, step) in dag_steps.iter().enumerate() {
        let step_id = step
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("step {} must have an id", idx));
        let instruction = step
            .get("task")
            .and_then(|t| t.as_str())
            .unwrap_or_else(|| panic!("step {} must have a task string", idx));
        let image = step
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or("stronghold/rocky-base:latest");
        let ttl_secs = step
            .get("ttl_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        let role = step.get("role").and_then(|v| v.as_str());

        let task_id = format!("task_{}", ulid::Ulid::new());

        // 6a. Build the spec blob. Same shape as `CreateTaskRequest` →
        //     `tasks.spec` in `routes/tasks.rs`.
        let mut spec = serde_json::json!({
            "instruction": instruction,
            "image": image,
            "ttl_secs": ttl_secs,
            "context": { "step_id": step_id, "workflow_run_id": run_id },
        });
        if let Some(r) = role {
            spec["role"] = serde_json::Value::String(r.to_string());
        }
        let spec_str = spec.to_string();

        // 6b. Insert the task as `queued`, scoped to the workflow run.
        {
            let conn = pool.get().expect("pool.get (insert task) must succeed");
            conn.execute(
                "INSERT INTO tasks
                 (id, tenant_id, machine_id, parent_task_id, workflow_run_id,
                  status, spec, result, created_at, started_at, finished_at,
                  error, retry_count, max_retries)
                 VALUES (?1, ?2, NULL, NULL, ?3, 'queued', ?4, NULL,
                         datetime('now'), NULL, NULL, NULL, 0, 3)",
                params![task_id, tenant.id, run_id, spec_str],
            )
            .expect("INSERT into tasks must succeed");
        }
        task_ids.push(task_id.clone());

        // 6c. Verify the task is `queued` immediately after insert.
        {
            let conn = pool.get().expect("pool.get (verify queued) must succeed");
            let status: String = conn
                .query_row(
                    "SELECT status FROM tasks WHERE id = ?1 AND tenant_id = ?2",
                    params![task_id, tenant.id],
                    |row| row.get(0),
                )
                .expect("queued task must be readable");
            assert_eq!(status, "queued", "step {} task must start queued", step_id);
        }

        // 6d. Transition queued → running (the scheduler picks it up).
        {
            let conn = pool.get().expect("pool.get (-> running) must succeed");
            let updated = conn.execute(
                "UPDATE tasks
                 SET status = 'running', started_at = datetime('now')
                 WHERE id = ?1 AND tenant_id = ?2 AND status = 'queued'",
                params![task_id, tenant.id],
            )
            .expect("UPDATE to running must succeed");
            assert_eq!(
                updated, 1,
                "exactly one row should transition queued -> running for step {}",
                step_id
            );
        }

        // 6e. Submit a successful result: running → completed. The result
        //     JSON includes `exit_code: 0` so downstream `condition`s
        //     (`<step>.result.exit_code == 0`) would evaluate true if the
        //     engine were driving the run.
        let result_json = serde_json::json!({
            "exit_code": 0,
            "stdout": format!("[{}] step {} ok", step_id, step_id),
            "stderr": "",
            "summary": format!("step {} completed successfully", step_id),
            "artifacts": [{
                "step_id": step_id,
                "produced": format!("artifact_for_{}", step_id),
            }],
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
                "exactly one row should transition running -> completed for step {}",
                step_id
            );
        }

        // 6f. Write an audit entry for the completion — proves the
        //     hash-chained audit log wires up across the workflow.
        let audit_payload = serde_json::json!({
            "task_id": task_id,
            "workflow_run_id": run_id,
            "step_id": step_id,
            "exit_code": 0,
            "status": "completed",
        });
        audit_log::entry(
            &pool,
            &tenant.id,
            "", // machine_id: tasks are not assigned a machine in this test
            "task_completed",
            audit_payload,
            &audit_keys,
        )
        .expect("audit::log::entry must succeed");

        completed_steps.push(step_id.to_string());

        // 6g. Update the run's `completed_steps` array to reflect progress.
        //     `current_steps` is emptied between waves (matching the engine).
        let completed_json = serde_json::to_string(&completed_steps)
            .expect("completed_steps JSON must serialize");
        {
            let conn = pool.get().expect("pool.get (update run progress) must succeed");
            conn.execute(
                "UPDATE workflow_runs
                 SET current_steps = '[]', completed_steps = ?1
                 WHERE id = ?2",
                params![completed_json, run_id],
            )
            .expect("UPDATE workflow_runs progress must succeed");
        }
    }

    // ----------------------------------------------------------------
    // 7. Finalise the workflow run → status = 'completed', finished_at set.
    //    Mirror `engine::finalize_run` exactly.
    // ----------------------------------------------------------------
    {
        let conn = pool.get().expect("pool.get (finalize run) must succeed");
        conn.execute(
            "UPDATE workflow_runs
             SET status = 'completed',
                 finished_at = datetime('now'),
                 current_steps = '[]'
             WHERE id = ?1",
            params![run_id],
        )
        .expect("UPDATE workflow_runs to completed must succeed");
    }

    // Write the `workflow_completed` audit entry (the engine does this too).
    audit_log::entry(
        &pool,
        &tenant.id,
        "",
        "workflow_completed",
        serde_json::json!({
            "run_id": run_id,
            "workflow_id": workflow_id,
            "tenant_id": tenant.id,
            "status": "completed",
            "step_count": dag_steps.len(),
        }),
        &audit_keys,
    )
    .expect("workflow_completed audit entry must succeed");

    // ----------------------------------------------------------------
    // 8. Verify the workflow run is now `completed` and all 4 tasks exist
    //    with `status = 'completed'`. Verify the run's `completed_steps`
    //    array contains all 4 step IDs.
    // ----------------------------------------------------------------
    {
        let conn = pool.get().expect("pool.get (verify run completed) must succeed");
        let (status, completed_steps_str, finished_at): (String, String, Option<String>) = conn
            .query_row(
                "SELECT status, completed_steps, finished_at
                 FROM workflow_runs WHERE id = ?1 AND tenant_id = ?2",
                params![run_id, tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("workflow_runs row must be readable");
        assert_eq!(
            status, "completed",
            "workflow run must be completed after all steps finish"
        );
        assert!(
            finished_at.is_some(),
            "finished_at must be stamped when the run completes"
        );

        let completed_steps_val: serde_json::Value =
            serde_json::from_str(&completed_steps_str).expect("completed_steps must be JSON");
        let completed_arr = completed_steps_val
            .as_array()
            .expect("completed_steps must be a JSON array");
        let mut actual: Vec<String> = completed_arr
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        actual.sort();
        let mut expected: Vec<String> = STEP_IDS.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(
            actual, expected,
            "run's completed_steps must contain all 4 step IDs"
        );
    }

    // Verify all 4 tasks exist with status = 'completed' and have results.
    {
        let conn = pool.get().expect("pool.get (verify tasks) must succeed");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks
                 WHERE workflow_run_id = ?1 AND tenant_id = ?2 AND status = 'completed'",
                params![run_id, tenant.id],
                |row| row.get(0),
            )
            .expect("COUNT of completed tasks must succeed");
        assert_eq!(
            count,
            STEP_IDS.len() as i64,
            "all 4 workflow tasks must be completed"
        );

        // Every completed task must have a non-null result JSON.
        let null_result_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks
                 WHERE workflow_run_id = ?1 AND tenant_id = ?2 AND result IS NULL",
                params![run_id, tenant.id],
                |row| row.get(0),
            )
            .expect("COUNT of null-result tasks must succeed");
        assert_eq!(
            null_result_count, 0,
            "no completed workflow task may have a NULL result"
        );

        // Each task's result must round-trip as JSON with exit_code 0.
        let mut stmt = conn
            .prepare(
                "SELECT id, result FROM tasks
                 WHERE workflow_run_id = ?1 AND tenant_id = ?2
                 ORDER BY created_at",
            )
            .expect("prepare task query must succeed");
        let rows: Vec<(String, String)> = stmt
            .query_map(params![run_id, tenant.id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query_map must succeed")
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), STEP_IDS.len());
        for (tid, result_str) in &rows {
            let result: serde_json::Value =
                serde_json::from_str(result_str).expect("task result must be valid JSON");
            assert_eq!(
                result["exit_code"], 0,
                "task {} result must have exit_code 0",
                tid
            );
            assert!(
                result["summary"].is_string(),
                "task {} result must have a summary",
                tid
            );
        }
    }

    // ----------------------------------------------------------------
    // 9. Submit a reflexion for the final task and verify it's retrievable.
    //    The reflexion is stored in `task_outputs` under the constant key
    //    `"reflexion"` (one per task; resubmission overwrites). Mirror the
    //    `POST /agent/task/:id/reflexion` handler's storage shape exactly.
    // ----------------------------------------------------------------
    let final_task_id = task_ids.last().expect("must have at least one task");
    let reflexion_body = serde_json::json!({
        "what_went_well": "Plan was clear; implementation went smoothly; tests passed first try.",
        "what_went_wrong": "Initial clone was slower than expected due to large history.",
        "what_differently": "Use a shallow clone (--depth 1) for ephemeral CI runs.",
        "what_learned": "Workflow conditions on exit_code are a clean way to gate downstream steps.",
        "tenant_id": tenant.id,
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    let reflexion_str = reflexion_body.to_string();
    {
        let conn = pool.get().expect("pool.get (insert reflexion) must succeed");
        conn.execute(
            "INSERT OR REPLACE INTO task_outputs (task_id, key, value, artifact_path)
             VALUES (?1, 'reflexion', ?2, NULL)",
            params![final_task_id, reflexion_str],
        )
        .expect("INSERT into task_outputs (reflexion) must succeed");
    }

    // Retrieve the reflexion via the same JOIN the GET handler uses
    // (task_outputs ⋈ tasks, scoped to the tenant).
    {
        let conn = pool.get().expect("pool.get (verify reflexion) must succeed");
        let (fetched_task_id, fetched_value): (String, String) = conn
            .query_row(
                "SELECT outs.task_id, outs.value
                 FROM task_outputs outs
                 JOIN tasks t ON outs.task_id = t.id
                 WHERE outs.task_id = ?1
                   AND outs.key = 'reflexion'
                   AND t.tenant_id = ?2",
                params![final_task_id, tenant.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("reflexion row must be retrievable");
        assert_eq!(fetched_task_id, *final_task_id);

        let fetched: serde_json::Value =
            serde_json::from_str(&fetched_value).expect("reflexion value must be valid JSON");
        assert_eq!(
            fetched["what_went_well"],
            reflexion_body["what_went_well"],
            "reflexion what_went_well must round-trip"
        );
        assert_eq!(
            fetched["what_went_wrong"],
            reflexion_body["what_went_wrong"],
            "reflexion what_went_wrong must round-trip"
        );
        assert_eq!(
            fetched["what_differently"],
            reflexion_body["what_differently"],
            "reflexion what_differently must round-trip"
        );
        assert_eq!(
            fetched["what_learned"],
            reflexion_body["what_learned"],
            "reflexion what_learned must round-trip"
        );
    }

    // ----------------------------------------------------------------
    // 10. Verify the audit log: one entry per task completion + one for the
    //     workflow completion = 5 entries total. The chain must be
    //     contiguous (each entry's `prev_hash` equals the prior `hash`).
    // ----------------------------------------------------------------
    {
        let conn = pool.get().expect("pool.get (verify audit chain) must succeed");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_entries WHERE tenant_id = ?1",
                params![tenant.id],
                |row| row.get(0),
            )
            .expect("audit count query must succeed");
        assert_eq!(
            count,
            (STEP_IDS.len() as i64) + 1, // 4 task_completed + 1 workflow_completed
            "audit log must contain one entry per task + one for the workflow"
        );

        // The chain must be contiguous: every non-first entry's prev_hash
        // equals the previous entry's hash.
        let mut stmt = conn
            .prepare(
                "SELECT prev_hash, hash FROM audit_entries
                 WHERE tenant_id = ?1 ORDER BY seq",
            )
            .expect("prepare audit chain query must succeed");
        let chain: Vec<(String, String)> = stmt
            .query_map(params![tenant.id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query_map audit chain must succeed")
            .map(|r| r.unwrap())
            .collect();
        for i in 1..chain.len() {
            assert_eq!(
                chain[i].0, chain[i - 1].1,
                "audit entry {} prev_hash must equal entry {} hash (chain broken)",
                i + 1,
                i
            );
        }
    }

    // ----------------------------------------------------------------
    // 11. Assert the whole E2E flow completed in under MAX_ELAPSED.
    // ----------------------------------------------------------------
    let elapsed = start.elapsed();
    assert!(
        elapsed < MAX_ELAPSED,
        "E2E multi-agent workflow took {:?}, expected < {:?}",
        elapsed,
        MAX_ELAPSED
    );

    eprintln!(
        "T1 e2e_multi_agent_workflow_full_flow: tenant + credential + 4-step workflow + 4 tasks + reflexion + audit chain in {:?} (< {:?})",
        elapsed, MAX_ELAPSED
    );
}
