//! T3: Watchdog system integration test.
//!
//! Exercises the real watchdog primitives against an in-memory SQLite
//! database:
//!
//! 1. Create tenant + task + machine in DB.
//! 2. Insert mock audit entries simulating an off-task agent (commands that
//!    don't match the task keywords).
//! 3. Call `compute_dedication()` — assert `score < 0.3` (agent is off-task).
//! 4. Call `detect_workarounds()` with a git diff containing `.unwrap()` and
//!    `#[allow(clippy::…)]` — assert ≥ 2 warnings are produced.
//! 5. Call `issue_ultimatum()` — Level 1 (Warning) is fire-and-forget by
//!    design (not persisted); we additionally issue Level 2 (Directive) and
//!    verify a row is stored in the `ultimata` table with `acknowledged = 0`.
//! 6. Insert a mock audit entry whose payload contains `echo ACK_TASK_FOCUS`
//!    (the acknowledgment sentinel).
//! 7. Call `check_ultimatum_acknowledgment()` — assert `true`.
//!
//! Run with:
//!     cargo test --workspace --features no-sev-snp --test watchdog_test

use std::sync::Arc;

use rusqlite::params;
use stronghold_gateway::crypto::hybrid_kem::PushKeys;
use stronghold_gateway::crypto::hybrid_sig::AuditKeys;
use stronghold_gateway::db::init_memory_pool;
use stronghold_gateway::routes::AppState;
use stronghold_gateway::tenants::{auth, quotas, registry};
use stronghold_gateway::watchdog::dedication::{
    compute_dedication, AuditEntryRef, ProgressIndicators,
};
use stronghold_gateway::watchdog::detector::detect_workarounds;
use stronghold_gateway::watchdog::ultimatum::{
    check_ultimatum_acknowledgment, issue_ultimatum, Ultimatum, UltimatumLevel,
};

/// The acknowledgment sentinel the agent must echo back. Mirrors the const
/// in `watchdog/ultimatum.rs` (kept private there, so we redefine it here
/// for test readability).
const ACK_TOKEN: &str = "ACK_TASK_FOCUS";

/// Build an `AppState` backed by an in-memory DB + freshly generated keys.
fn setup_state() -> AppState {
    let pool = init_memory_pool().expect("init_memory_pool must succeed");
    AppState {
        db: pool,
        audit_keys: AuditKeys::generate(),
        push_keys: PushKeys::generate(),
        pty_registry: Arc::new(tokio::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
    }
}

/// Insert a tenant + machine + running task into the DB and return
/// `(tenant_id, machine_id, task_id)`.
///
/// The task instruction contains recognisable keywords ("auth", "token",
/// "expiry") so the dedication scorer has something to match against (and
/// the off-task audit entries in step 2 deliberately don't use them).
fn seed_tenant_machine_task(state: &AppState) -> (String, String, String) {
    let tenant = registry::create(&state.db, "watchdog-tenant")
        .expect("registry::create must succeed");
    quotas::set(&state.db, &tenant.id, 5, 8, 16).expect("quotas::set must succeed");
    let _token = auth::mint_agent_token(&state.db, &tenant.id, "default", 3600)
        .expect("mint_agent_token must succeed");

    // Insert a machine row.
    let machine_id = format!("mach_{}", ulid::Ulid::new());
    {
        let conn = state.db.get().expect("pool.get (insert machine) must succeed");
        conn.execute(
            "INSERT INTO machines
             (id, tenant_id, image, worker, status, cpu, memory_gb,
              created_at, expires_at)
             VALUES (?1, ?2, 'stronghold/rust-nightly:latest', 'worker-1',
                     'active', 4, 8, datetime('now'),
                     datetime('now', '+1 hour'))",
            params![machine_id, tenant.id],
        )
        .expect("INSERT into machines must succeed");
    }

    // Insert a running task assigned to that machine. The instruction
    // contains the keywords the dedication scorer should look for.
    let task_id = format!("task_{}", ulid::Ulid::new());
    let spec = serde_json::json!({
        "instruction": "Fix the auth token expiry bug in src/auth.rs — add exp claim validation",
        "image": "stronghold/rust-nightly:latest",
        "ttl_secs": 1800u64,
        "context": { "component": "auth" },
    });
    let spec_str = spec.to_string();
    {
        let conn = state.db.get().expect("pool.get (insert task) must succeed");
        conn.execute(
            "INSERT INTO tasks
             (id, tenant_id, machine_id, parent_task_id, workflow_run_id,
              status, spec, result, created_at, started_at, finished_at,
              error, retry_count, max_retries)
             VALUES (?1, ?2, ?3, NULL, NULL, 'running', ?4, NULL,
                     datetime('now'), datetime('now'), NULL, NULL, 0, 3)",
            params![task_id, tenant.id, machine_id, spec_str],
        )
        .expect("INSERT into tasks must succeed");
    }

    (tenant.id, machine_id, task_id)
}

/// Read recent audit entries for a machine back as `AuditEntryRef`s.
///
/// Mirrors the `get_recent_audit_entries` helper in
/// `watchdog/monitor.rs` — same SQL, same projection — so the test exercises
/// the same data path the production watchdog uses.
fn get_recent_audit_entries(
    state: &AppState,
    machine_id: &str,
) -> Vec<AuditEntryRef> {
    let conn = state
        .db
        .get()
        .expect("pool.get (audit read) must succeed");
    let mut stmt = conn
        .prepare(
            "SELECT event, payload FROM audit_entries
             WHERE machine_id = ?1
             ORDER BY seq DESC LIMIT 50",
        )
        .expect("prepare audit query must succeed");
    let rows = stmt
        .query_map(params![machine_id], |row| {
            let event: String = row.get(0)?;
            let payload_str: String = row.get(1)?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
            let cmd = payload
                .get("cmd")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(AuditEntryRef { cmd, event, payload })
        })
        .expect("query_map audit entries must succeed");
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row.expect("audit row must decode"));
    }
    // query_map returns DESC (newest first); the scorer doesn't care about
    // order, but reverse to chronological for readability if needed.
    entries.reverse();
    entries
}

/// End-to-end watchdog integration: low dedication → workarounds detected →
/// ultimatum issued → agent acknowledges → ultimatum cleared.
#[tokio::test]
async fn watchdog_full_integration_flow() {
    let state = setup_state();

    // ----------------------------------------------------------------
    // 1. Create tenant + task + machine.
    // ----------------------------------------------------------------
    let (tenant_id, machine_id, task_id) = seed_tenant_machine_task(&state);

    // ----------------------------------------------------------------
    // 2. Insert mock audit entries simulating an off-task agent.
    //    The agent is *busy* (4 commands in the last window) but none of
    //    the commands match the task keywords ("auth", "token", "expiry").
    // ----------------------------------------------------------------
    let off_task_commands = [
        "ls -la /tmp",
        "cat /etc/hostname",
        "echo hello world",
        "git log --oneline -5",
    ];
    for cmd in &off_task_commands {
        stronghold_gateway::audit::log::entry(
            &state.db,
            &tenant_id,
            &machine_id,
            "cmd_exec",
            serde_json::json!({
                "cmd": cmd,
                "args": serde_json::Value::Array(vec![]),
                "exit_code": 0,
            }),
            &state.audit_keys,
        )
        .expect("audit::log::entry (off-task) must succeed");
    }

    // Read them back as AuditEntryRefs (same path as the production monitor).
    let entries = get_recent_audit_entries(&state, &machine_id);
    assert_eq!(
        entries.len(),
        off_task_commands.len(),
        "must read back exactly the off-task audit entries we wrote"
    );

    // ----------------------------------------------------------------
    // 3. Call compute_dedication() — assert score < 0.3.
    //    Keywords derived from the task instruction (words > 3 chars,
    //    lowercased) — same heuristic as `monitor::extract_keywords`.
    // ----------------------------------------------------------------
    let task_keywords: Vec<String> = ["auth", "token", "expiry", "validation"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Derive progress indicators from the audit entries (same path as the
    // production monitor). The off-task commands don't include any
    // git-diff/git-add/git-commit/cargo-test/pytest patterns, so progress
    // should be 0.
    let progress = ProgressIndicators::from_audit_entries(&entries);
    assert_eq!(progress.files_changed, 0, "off-task commands touch no files");
    assert_eq!(progress.tests_run, 0, "off-task commands run no tests");
    assert_eq!(progress.commits, 0, "off-task commands make no commits");

    let score = compute_dedication(&entries, &task_keywords, &progress);

    // With 0 relevant commands out of 4 total, task_alignment = 0.5 (there
    // IS activity, just none of it relevant), and progress_rate = 0.0:
    //   score = (0/4) * 0.0 * 0.5 = 0.0
    assert_eq!(
        score.total_commands, 4,
        "scorer must see all 4 off-task commands"
    );
    assert_eq!(
        score.relevant_commands, 0,
        "no off-task command should match the task keywords"
    );
    assert!(
        score.score < 0.3,
        "off-task agent dedication ({:.4}) must be < 0.3 to trigger the watchdog",
        score.score
    );
    // The score should actually be exactly 0.0 — no relevant commands AND
    // no progress. Assert the stronger bound for a tighter contract.
    assert!(
        score.score <= 0.01,
        "off-task + no-progress agent should score ~0.0, got {:.4}",
        score.score
    );

    // ----------------------------------------------------------------
    // 4. Call detect_workarounds() with a git diff containing .unwrap()
    //    and #[allow(clippy::…)]. Assert ≥ 2 warnings are produced.
    //
    //    The detector scans ONLY added lines (those beginning with `+`,
    //    excluding the `+++ b/file` header), so we craft a realistic diff
    //    with the offending patterns on added lines.
    // ----------------------------------------------------------------
    let git_diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,5 @@\n fn main() {\n+    let x = option.unwrap();\n+    #[allow(clippy::needless_return)]\n }\n";
    let warnings = detect_workarounds("", git_diff);

    let patterns: Vec<&str> = warnings.iter().map(|w| w.pattern.as_str()).collect();
    assert!(
        warnings.len() >= 2,
        "expected ≥ 2 workaround warnings (unwrap + allow_clippy), got {}: {:?}",
        warnings.len(),
        patterns
    );
    assert!(
        patterns.contains(&"unwrap_call"),
        "warnings must include `unwrap_call` (got {:?})",
        patterns
    );
    assert!(
        patterns.contains(&"allow_clippy"),
        "warnings must include `allow_clippy` (got {:?})",
        patterns
    );

    // ----------------------------------------------------------------
    // 5. Call issue_ultimatum() at Level 1 (Warning) — verify it returns
    //    Ok. By design (see `ultimatum.rs` line ~157), Level 1 is
    //    fire-and-forget: the warning is injected via PTY (if a session
    //    exists) and NOT persisted in the `ultimata` table.
    //
    //    To test the persistence + acknowledgment flow (steps 6-7), we
    //    additionally issue a Level 2 (Directive) ultimatum, which IS
    //    persisted with `acknowledged = 0`. We verify both behaviours
    //    explicitly below.
    // ----------------------------------------------------------------
    let level1_ultimatum = Ultimatum {
        level: UltimatumLevel::Warning,
        target_machine: machine_id.clone(),
        target_task_id: Some(task_id.clone()),
        message: "You appear to be off-task. Please refocus on the assigned task.".to_string(),
        deadline_seconds: None,
    };
    issue_ultimatum(&state, &level1_ultimatum)
        .await
        .expect("issue_ultimatum (Level 1) must succeed");

    // Level 1 is fire-and-forget → no row in `ultimata`.
    {
        let conn = state.db.get().expect("pool.get (count L1) must succeed");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ultimata WHERE target_machine = ?1 AND level = 1",
                params![machine_id],
                |row| row.get(0),
            )
            .expect("COUNT of Level 1 ultimata must succeed");
        assert_eq!(
            count, 0,
            "Level 1 (Warning) must NOT be persisted — it's fire-and-forget by design"
        );
    }

    // Issue Level 2 (Directive) — this IS persisted.
    let level2_ultimatum = Ultimatum {
        level: UltimatumLevel::Directive,
        target_machine: machine_id.clone(),
        target_task_id: Some(task_id.clone()),
        message: format!(
            "You must refocus on the assigned task. Acknowledge by running: echo {}",
            ACK_TOKEN
        ),
        deadline_seconds: Some(120),
    };
    issue_ultimatum(&state, &level2_ultimatum)
        .await
        .expect("issue_ultimatum (Level 2) must succeed");

    // Verify the Level 2 row exists with the correct fields.
    let ultimatum_id: i64 = {
        let conn = state.db.get().expect("pool.get (verify L2) must succeed");
        let (id, level, ack, target_machine, target_task_id, message): (
            i64,
            i64,
            i64,
            String,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT id, level, acknowledged, target_machine, target_task_id, message
                 FROM ultimata
                 WHERE target_machine = ?1 AND level = 2
                 ORDER BY id DESC LIMIT 1",
                params![machine_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("Level 2 ultimatum row must be readable");
        assert_eq!(level, 2, "persisted ultimatum level must be 2 (Directive)");
        assert_eq!(
            ack, 0,
            "newly-issued ultimatum must start with acknowledged = 0"
        );
        assert_eq!(target_machine, machine_id);
        assert_eq!(target_task_id.as_deref(), Some(task_id.as_str()));
        assert!(
            message.contains(ACK_TOKEN),
            "Level 2 message must contain the ACK token (got: {})",
            message
        );
        id
    };

    // ----------------------------------------------------------------
    // 6. Insert a mock audit entry with "echo ACK_TASK_FOCUS".
    //
    //    The watchdog's `check_ultimatum_acknowledgment` looks for
    //    audit_entries WHERE machine_id = target AND event = 'cmd_exec' AND
    //    payload LIKE '%ACK_TASK_FOCUS%' AND datetime(ts) > datetime(created_at).
    //
    //    The ultimatum's `created_at` is `datetime('now')` (second
    //    precision). The audit entry's `ts` is `chrono::Utc::now()` (RFC
    //    3339, microsecond precision). If both fall in the same UTC second,
    //    the strict `>` comparison can fail. We backdate the ultimatum's
    //    `created_at` by 1 minute (same approach as the existing unit test
    //    `seed_ultimatum_backdated` in `ultimatum.rs`) to avoid the race.
    // ----------------------------------------------------------------
    {
        let conn = state
            .db
            .get()
            .expect("pool.get (backdate ultimatum) must succeed");
        let updated = conn.execute(
            "UPDATE ultimata
             SET created_at = datetime('now', '-1 minute')
             WHERE id = ?1",
            params![ultimatum_id],
        )
        .expect("backdate UPDATE must succeed");
        assert_eq!(updated, 1, "must backdate exactly one ultimatum row");
    }

    // Now write the ACK audit entry — `ts = now`, strictly after the
    // backdated `created_at`.
    stronghold_gateway::audit::log::entry(
        &state.db,
        &tenant_id,
        &machine_id,
        "cmd_exec",
        serde_json::json!({
            "cmd": "echo",
            "args": [ACK_TOKEN],
            "exit_code": 0,
        }),
        &state.audit_keys,
    )
    .expect("audit::log::entry (ACK) must succeed");

    // ----------------------------------------------------------------
    // 7. Call check_ultimatum_acknowledgment() — assert true.
    // ----------------------------------------------------------------
    let acknowledged = check_ultimatum_acknowledgment(&state.db, ultimatum_id);
    assert!(
        acknowledged,
        "ultimatum must be acknowledged after the ACK_TASK_FOCUS audit entry"
    );

    eprintln!(
        "T3 watchdog_full_integration_flow: tenant + machine + task + 4 off-task audit entries + \
         dedication={:.4} + {} workaround warnings + Level 1 (not persisted) + Level 2 (persisted) + \
         ACK detected = {}",
        score.score,
        warnings.len(),
        acknowledged
    );
}

/// Sanity-check the dedication scorer in isolation: a fully on-task agent
/// (every command matches a keyword, with progress) must score ≥ 0.3, and
/// an empty audit window must score 0.0. This guards against the
/// integration test above passing for the wrong reason (e.g. the scorer
/// always returning 0).
#[test]
fn dedication_scorer_bounds_check() {
    let keywords: Vec<String> = vec!["auth".to_string(), "token".to_string()];

    // Empty audit → score 0.0.
    let empty_progress = ProgressIndicators {
        files_changed: 0,
        tests_run: 0,
        commits: 0,
        last_activity_secs: 0,
    };
    let empty_score = compute_dedication(&[], &keywords, &empty_progress);
    assert_eq!(empty_score.score, 0.0);
    assert_eq!(empty_score.task_alignment, 0.0);

    // All-relevant + progress → high score.
    let on_task = vec![
        AuditEntryRef {
            cmd: "cargo build -p auth".to_string(),
            event: "cmd_exec".to_string(),
            payload: serde_json::json!({}),
        },
        AuditEntryRef {
            cmd: "cargo test auth::token".to_string(),
            event: "cmd_exec".to_string(),
            payload: serde_json::json!({}),
        },
    ];
    let good_progress = ProgressIndicators {
        files_changed: 2,
        tests_run: 1,
        commits: 1,
        last_activity_secs: 10,
    };
    let on_task_score = compute_dedication(&on_task, &keywords, &good_progress);
    assert!(
        on_task_score.score >= 0.3,
        "on-task agent with progress should score ≥ 0.3, got {:.4}",
        on_task_score.score
    );
    assert_eq!(on_task_score.relevant_commands, 2);
    assert_eq!(on_task_score.total_commands, 2);
}

/// Sanity-check the workaround detector in isolation: a clean diff must
/// produce zero warnings, and a diff with multiple shortcuts must produce
/// multiple distinct warnings. Guards against the integration test passing
/// because the detector always returns ≥ 2 warnings.
#[test]
fn workaround_detector_bounds_check() {
    // Clean diff → no warnings. Note: `println!` IS detected as a
    // medium-severity workaround, so we use a documentation-only diff to
    // get a truly clean baseline.
    let truly_clean = "--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n# Project\n+Some documentation.\n";
    let clean_warnings = detect_workarounds("", truly_clean);
    assert_eq!(
        clean_warnings.len(),
        0,
        "clean diff should produce 0 warnings, got: {:?}",
        clean_warnings
    );

    // Diff with 3 distinct shortcuts → ≥ 3 warnings.
    let dirty_diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,6 @@\n fn main() {\n+    let x = option.unwrap();\n+    #[allow(clippy::needless_return)]\n+    todo!();\n }\n";
    let dirty_warnings = detect_workarounds("", dirty_diff);
    assert!(
        dirty_warnings.len() >= 3,
        "dirty diff should produce ≥ 3 warnings (unwrap + allow_clippy + todo), got {}: {:?}",
        dirty_warnings.len(),
        dirty_warnings
            .iter()
            .map(|w| w.pattern.as_str())
            .collect::<Vec<_>>()
    );
}
