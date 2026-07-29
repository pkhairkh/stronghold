#![allow(clippy::doc_overindented_list_items)]
//! DAG executor — walks a workflow's directed acyclic graph and runs each
//! step as a Task.
//!
//! # DAG format
//! The `workflows.dag` column stores a JSON object:
//! ```json
//! {
//!   "steps": [
//!     {
//!       "id": "build",
//!       "task": "cargo build --release",
//!       "image": "stronghold/rust-nightly:latest",
//!       "depends_on": [],
//!       "condition": null,
//!       "max_retries": 3
//!     },
//!     {
//!       "id": "test",
//!       "task": "cargo test",
//!       "depends_on": ["build"],
//!       "condition": "build.result.exit_code == 0"
//!     }
//!   ]
//! }
//! ```
//!
//! # Execution model
//! 1. Load the [`Dag`] from the `workflows` table (via the run's `workflow_id`).
//! 2. Mark the run `running`.
//! 3. Loop:
//!    a. Find **ready** steps — not yet completed, all `depends_on` in
//!       `completed_steps`.
//!    b. Partition ready steps into **run** (condition met / no condition) and
//!       **skip** (condition not met). Skipped steps are recorded as completed
//!       with a `{"result":{"skipped":true}}` placeholder so downstream steps
//!       can proceed.
//!    c. Spawn one [`tokio::task`] per step to run. Each inserts a row into
//!       `tasks` with `status='queued'` and `workflow_run_id=<run_id>`, then
//!       polls every 1 s (up to 30 min per attempt) for a terminal status.
//!    d. On `completed`: store the result, add to `completed_steps`.
//!       On `failed` (after `max_retries`): store the result, mark the run
//!       `failed` and stop.
//!    e. Update `workflow_runs.current_steps` / `completed_steps` after each
//!       batch.
//! 4. When all steps are completed (or skipped), mark the run `completed`.
//! 5. Write an audit entry (`workflow_completed` / `workflow_failed`).
//!
//! # Condition language
//! Conditions are simple `<path> <op> <value>` strings where:
//! - `path` is a dot-separated lookup into the per-step results map, e.g.
//!   `build.result.exit_code` → `results["build"]["result"]["exit_code"]`.
//! - `op` is `==` or `!=`.
//! - `value` is an integer, boolean, or quoted string literal.
//!
//! An empty / missing condition is always true (the step always runs).
//!
//! # Concurrency
//! Steps whose dependencies are all met launch concurrently via
//! [`tokio::spawn`]. The executor `await`s the whole batch before
//! re-evaluating the DAG, so a new wave of ready steps is identified only
//! after the current wave finishes.

use crate::routes::AppState;
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How long a single task attempt may run before the executor gives up on it.
const STEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Poll interval while waiting for a task to reach a terminal status.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

// ============================================================================
// DAG types
// ============================================================================

/// Top-level DAG definition stored in `workflows.dag`.
#[derive(Debug, Clone, Deserialize)]
pub struct Dag {
    /// Ordered list of steps. Order is not semantically significant — the
    /// executor resolves the execution order from `depends_on` — but is
    /// preserved for deterministic test output.
    pub steps: Vec<Step>,
}

/// One node in the workflow DAG.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    /// Unique step identifier within the workflow. Referenced by other steps'
    /// `depends_on` and by conditions.
    pub id: String,
    /// Natural-language or shell instruction the agent executes.
    pub task: String,
    /// OCI image to run the step in. Defaults to `stronghold/rocky-base:latest`
    /// when omitted.
    #[serde(default)]
    pub image: Option<String>,
    /// Time-to-live for each attempt, in seconds. Defaults to 3600 (1 h).
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// Free-form context (env, prior outputs, …) passed to the agent.
    #[serde(default)]
    pub context: Option<serde_json::Value>,
    /// IDs of steps that must complete before this step can start.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Optional condition gating whether the step runs after its dependencies
    /// complete. See the module docs for the syntax.
    #[serde(default)]
    pub condition: Option<String>,
    /// Maximum retry attempts after the first failure. Defaults to 3.
    #[serde(default)]
    pub max_retries: Option<i64>,
}

impl Step {
    /// Effective `max_retries` — the configured value or the default of 3.
    fn effective_max_retries(&self) -> i64 {
        self.max_retries.unwrap_or(3)
    }

    /// Effective image — the configured value or the rocky base default.
    fn effective_image(&self) -> &str {
        self.image.as_deref().unwrap_or("stronghold/rocky-base:latest")
    }

    /// Effective TTL in seconds — the configured value or 1 hour.
    fn effective_ttl(&self) -> u64 {
        self.ttl_secs.unwrap_or(3600)
    }
}

// ============================================================================
// Public entry point
// ============================================================================

/// Execute a workflow run to completion (or failure).
///
/// Loads the DAG from the database, walks it in dependency order, launches
/// ready steps concurrently, polls the `tasks` table for each step's result,
/// and finally updates `workflow_runs.status` and writes an audit entry.
///
/// This function is designed to be `tokio::spawn`'d by the route handler —
/// it takes ownership of a cloned [`AppState`] and an owned `run_id` (the
/// public signature takes `&str` for ergonomic call sites; callers that
/// spawn it on a separate task must clone to a `String` first).
///
/// # Errors
/// Returns an error if:
/// - the run or workflow can't be loaded,
/// - a step fails after exhausting its retries,
/// - the DAG has a cycle (no ready steps, not all completed),
/// - a database error occurs.
pub async fn execute(run_id: &str, state: AppState) -> Result<()> {
    tracing::info!(run_id = run_id, "Workflow run starting");

    // 1. Load the run + workflow + DAG.
    let (workflow_id, tenant_id, dag_json) = load_run(&state.db, run_id)?;
    let dag: Dag = serde_json::from_str(&dag_json)
        .map_err(|e| anyhow!("Invalid DAG JSON for workflow {}: {}", workflow_id, e))?;

    // 2. Mark the run as running (with empty step arrays if not already set).
    set_run_running(&state.db, run_id)?;

    // 3. Walk the DAG.
    let outcome = execute_dag(run_id, &tenant_id, &dag, &state).await;

    // 4. Finalize the run status + timestamp.
    let (final_status, event) = match &outcome {
        Ok(()) => ("completed", "workflow_completed"),
        Err(e) => {
            tracing::error!(run_id = run_id, error = %e, "Workflow run failed");
            ("failed", "workflow_failed")
        }
    };
    finalize_run(&state.db, run_id, final_status)?;

    // 5. Audit entry — always written, even on failure, so the log reflects
    //    the terminal state of every run.
    let audit_payload = serde_json::json!({
        "run_id": run_id,
        "workflow_id": workflow_id,
        "tenant_id": tenant_id,
        "status": final_status,
        "step_count": dag.steps.len(),
    });
    crate::audit::log::entry(
        &state.db,
        &tenant_id,
        "",
        event,
        audit_payload,
        &state.audit_keys,
    )?;

    if let Err(e) = outcome {
        tracing::info!(run_id = run_id, status = final_status, "Workflow run finished (failed)");
        return Err(e);
    }
    tracing::info!(run_id = run_id, status = final_status, "Workflow run finished (completed)");
    Ok(())
}

// ============================================================================
// DAG walker
// ============================================================================

/// Drive the DAG to completion (or failure).
///
/// Maintains an in-memory `completed` set and `results` map. Each iteration
/// finds the next wave of ready steps, partitions them by condition, spawns
/// the runnable ones concurrently, and awaits the whole batch.
async fn execute_dag(
    run_id: &str,
    tenant_id: &str,
    dag: &Dag,
    state: &AppState,
) -> Result<()> {
    let mut completed: HashSet<String> = HashSet::new();
    let mut results: HashMap<String, serde_json::Value> = HashMap::new();

    while completed.len() < dag.steps.len() {
        // --- Find ready steps ---------------------------------------------
        // A step is "ready" when it hasn't run yet AND every step in its
        // `depends_on` list is in `completed`.
        let ready: Vec<&Step> = dag
            .steps
            .iter()
            .filter(|s| !completed.contains(&s.id))
            .filter(|s| dependencies_met(s, &completed))
            .collect();

        if ready.is_empty() {
            // No ready steps and not all completed → the DAG is stuck. This
            // happens when there's a cycle or a step depends on a step that
            // was skipped/failed and never made it into `completed`.
            return Err(anyhow!(
                "Workflow stuck: {} of {} steps completed, none ready",
                completed.len(),
                dag.steps.len()
            ));
        }

        // --- Partition by condition ---------------------------------------
        // Steps with an unmet condition are skipped (recorded as completed
        // with a placeholder result) so downstream steps can proceed.
        let mut to_run: Vec<&Step> = Vec::new();
        let mut to_skip: Vec<&Step> = Vec::new();
        for step in &ready {
            let cond = step.condition.as_deref().unwrap_or("");
            if evaluate_condition(cond, &results) {
                to_run.push(step);
            } else {
                to_skip.push(step);
            }
        }

        for step in &to_skip {
            tracing::info!(run_id = run_id, step = %step.id, "Step skipped (condition not met)");
            results.insert(
                step.id.clone(),
                serde_json::json!({ "result": { "skipped": true } }),
            );
            completed.insert(step.id.clone());
        }
        if !to_skip.is_empty() {
            update_step_arrays(&state.db, run_id, &[], &completed)?;
        }

        if to_run.is_empty() {
            // All ready steps were skipped — loop again to find the next wave.
            continue;
        }

        // --- Update current_steps -----------------------------------------
        let current_ids: Vec<&str> = to_run.iter().map(|s| s.id.as_str()).collect();
        update_step_arrays(&state.db, run_id, &current_ids, &completed)?;

        // --- Launch runnable steps concurrently ---------------------------
        let mut handles: Vec<(String, tokio::task::JoinHandle<Result<serde_json::Value>>)> =
            Vec::with_capacity(to_run.len());
        for step in to_run {
            let state_clone = state.clone();
            let tenant_id_clone = tenant_id.to_string();
            let run_id_clone = run_id.to_string();
            let step_clone = step.clone();
            let handle = tokio::spawn(async move {
                run_step_with_retries(&state_clone, &run_id_clone, &tenant_id_clone, &step_clone)
                    .await
            });
            handles.push((step.id.clone(), handle));
        }

        // --- Await the whole batch ----------------------------------------
        let mut batch_failed = false;
        for (step_id, handle) in handles {
            match handle.await {
                Ok(Ok(result)) => {
                    completed.insert(step_id.clone());
                    results.insert(step_id.clone(), result);
                    tracing::info!(run_id = run_id, step = %step_id, "Step completed");
                }
                Ok(Err(e)) => {
                    tracing::error!(run_id = run_id, step = %step_id, error = %e, "Step failed");
                    batch_failed = true;
                }
                Err(e) => {
                    tracing::error!(run_id = run_id, step = %step_id, error = %e, "Step task panicked");
                    batch_failed = true;
                }
            }
        }

        // Persist the updated completed_steps.
        update_step_arrays(&state.db, run_id, &[], &completed)?;

        if batch_failed {
            return Err(anyhow!("One or more steps failed"));
        }
    }

    Ok(())
}

// ============================================================================
// Step execution (with retries + polling)
// ============================================================================

/// Run a step, retrying on failure up to `max_retries` times.
///
/// Each attempt creates a fresh row in `tasks` (so each attempt has its own
/// immutable spec + result). Returns the result JSON of the successful
/// attempt, wrapped as `{"result": <task_result>}` for condition lookups.
async fn run_step_with_retries(
    state: &AppState,
    run_id: &str,
    tenant_id: &str,
    step: &Step,
) -> Result<serde_json::Value> {
    let max_retries = step.effective_max_retries();
    let mut last_error = String::new();

    // `attempt` counts from 0; 0 is the initial try, 1..=max_retries are
    // retries. Total attempts = max_retries + 1.
    for attempt in 0..=max_retries {
        if attempt > 0 {
            tracing::info!(run_id = run_id, step = %step.id, attempt = attempt, "Retrying step");
        }
        match run_step_once(state, run_id, tenant_id, step, attempt).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                last_error = e.to_string();
                tracing::warn!(
                    run_id = run_id,
                    step = %step.id,
                    attempt = attempt,
                    error = %e,
                    "Step attempt failed"
                );
            }
        }
    }

    Err(anyhow!(
        "Step {} failed after {} retries: {}",
        step.id,
        max_retries,
        last_error
    ))
}

/// Run a single attempt of a step: create a queued task, then poll for its
/// terminal status.
///
/// The task is inserted with `workflow_run_id = run_id` so it can be traced
/// back to this workflow run. Polls every [`POLL_INTERVAL`] until the task
/// reaches `completed` or `failed`, or until [`STEP_TIMEOUT`] elapses (which
/// is treated as a failure for retry purposes).
async fn run_step_once(
    state: &AppState,
    run_id: &str,
    tenant_id: &str,
    step: &Step,
    attempt: i64,
) -> Result<serde_json::Value> {
    // Build the task spec — same shape as `CreateTaskRequest` in routes/tasks.rs.
    let spec = serde_json::json!({
        "instruction": step.task,
        "image": step.effective_image(),
        "ttl_secs": step.effective_ttl(),
        "context": step.context,
    });
    let task_id = format!("task_{}", ulid::Ulid::new());

    {
        let conn = state
            .db
            .get()
            .map_err(|e| anyhow!("DB pool error: {}", e))?;
        conn.execute(
            "INSERT INTO tasks
             (id, tenant_id, workflow_run_id, status, spec, created_at,
              retry_count, max_retries)
             VALUES (?1, ?2, ?3, 'queued', ?4, datetime('now'), ?5, ?6)",
            params![
                task_id,
                tenant_id,
                run_id,
                spec.to_string(),
                attempt,
                step.effective_max_retries(),
            ],
        )
        .map_err(|e| anyhow!("Failed to insert task for step {}: {}", step.id, e))?;
    }

    tracing::info!(
        run_id = run_id,
        step = %step.id,
        task_id = %task_id,
        attempt = attempt,
        "Task queued for step"
    );

    // Poll the tasks table for a terminal status.
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            return Err(anyhow!(
                "Step {} timed out after {} seconds",
                step.id,
                STEP_TIMEOUT.as_secs()
            ));
        }

        let row: Option<(String, Option<String>)> = {
            let conn = state
                .db
                .get()
                .map_err(|e| anyhow!("DB pool error: {}", e))?;
            let r = conn.query_row(
                "SELECT status, result FROM tasks WHERE id = ?1",
                params![task_id],
                |row| {
                    let status: String = row.get(0)?;
                    let result: Option<String> = row.get(1)?;
                    Ok((status, result))
                },
            );
            match r {
                Ok(v) => Some(v),
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Err(anyhow!("Task {} vanished mid-poll", task_id));
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "Transient DB error polling task; will retry"
                    );
                    None
                }
            }
        };

        if let Some((status, result_str)) = row {
            match status.as_str() {
                "completed" => {
                    let result: serde_json::Value = result_str
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Null);
                    return Ok(serde_json::json!({ "result": result }));
                }
                "failed" | "cancelled" => {
                    let result: serde_json::Value = result_str
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Null);
                    let detail = result
                        .get("stderr")
                        .and_then(|v| v.as_str())
                        .or_else(|| result.get("summary").and_then(|v| v.as_str()))
                        .unwrap_or("unknown error");
                    return Err(anyhow!("Step {} attempt {} failed: {}", step.id, attempt, detail));
                }
                _ => {
                    // queued / scheduled / running — keep polling.
                }
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// ============================================================================
// Condition evaluation
// ============================================================================

/// Evaluate a condition string against the per-step results map.
///
/// Supported syntax:
/// - `""` (empty) → always true.
/// - `"<path> == <value>"` → true if the resolved value equals `value`.
/// - `"<path> != <value>"` → true if the resolved value does not equal `value`.
///
/// `<path>` is a dot-separated lookup, e.g. `build.result.exit_code` →
/// `results["build"]["result"]["exit_code"]`. `<value>` is an integer, a
/// boolean (`true`/`false`), or a quoted string.
///
/// Unparseable conditions or unresolvable paths evaluate to `false` (fail
/// safe — don't run the step).
fn evaluate_condition(condition: &str, results: &HashMap<String, serde_json::Value>) -> bool {
    let cond = condition.trim();
    if cond.is_empty() {
        return true;
    }

    // Try == first, then !=.
    if let Some((lhs, rhs)) = cond.split_once(" == ") {
        return match resolve_path(lhs.trim(), results) {
            Some(actual) => value_equals(actual, rhs.trim()),
            None => false,
        };
    }
    if let Some((lhs, rhs)) = cond.split_once(" != ") {
        return match resolve_path(lhs.trim(), results) {
            Some(actual) => !value_equals(actual, rhs.trim()),
            None => true,
        };
    }

    tracing::warn!(condition = cond, "Unparseable condition; failing safe (false)");
    false
}

/// Resolve a dot-separated path against the results map.
///
/// `build.result.exit_code` → `results["build"]["result"]["exit_code"]`.
/// Returns `None` if any segment is missing.
fn resolve_path<'a>(
    path: &str,
    results: &'a HashMap<String, serde_json::Value>,
) -> Option<&'a serde_json::Value> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() {
        return None;
    }
    let mut current = results.get(segments[0])?;
    for seg in &segments[1..] {
        current = current.get(seg)?;
    }
    Some(current)
}

/// Compare a resolved JSON value against an expected literal string.
///
/// - Numbers: parsed as `i64` (then `f64`) and compared numerically.
/// - Booleans: `"true"` / `"false"`.
/// - Strings: the raw expected text (quotes stripped if present).
fn value_equals(actual: &serde_json::Value, expected: &str) -> bool {
    match actual {
        serde_json::Value::Number(n) => {
            if let Ok(i) = expected.parse::<i64>() {
                return n.as_i64() == Some(i);
            }
            if let Ok(f) = expected.parse::<f64>() {
                return n.as_f64() == Some(f);
            }
            false
        }
        serde_json::Value::Bool(b) => match expected {
            "true" => *b,
            "false" => !*b,
            _ => false,
        },
        serde_json::Value::String(s) => {
            // Strip surrounding quotes if present.
            let exp = expected.trim_matches('"');
            s == exp
        }
        serde_json::Value::Null => expected == "null",
        _ => false,
    }
}

// ============================================================================
// Dependency resolution
// ============================================================================

/// True if every step in `step.depends_on` is present in `completed`.
fn dependencies_met(step: &Step, completed: &HashSet<String>) -> bool {
    step.depends_on.iter().all(|dep| completed.contains(dep))
}

/// Return the IDs of steps that are ready to run: not yet completed and with
/// all dependencies satisfied.
///
/// Exposed (pub(crate)) so tests in this module can drive the wave-by-wave
/// scheduling without a database.
pub(crate) fn find_ready_steps<'a>(
    steps: &'a [Step],
    completed: &HashSet<String>,
) -> Vec<&'a Step> {
    steps
        .iter()
        .filter(|s| !completed.contains(&s.id))
        .filter(|s| dependencies_met(s, completed))
        .collect()
}

// ============================================================================
// Database helpers
// ============================================================================

/// Load a workflow run: returns `(workflow_id, tenant_id, dag_json)`.
fn load_run(
    db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    run_id: &str,
) -> Result<(String, String, String)> {
    let conn = db.get().map_err(|e| anyhow!("DB pool error: {}", e))?;
    let (workflow_id, tenant_id): (String, String) = conn
        .query_row(
            "SELECT workflow_id, tenant_id FROM workflow_runs WHERE id = ?1",
            params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| anyhow!("Failed to load workflow run {}: {}", run_id, e))?;

    let dag_json: String = conn
        .query_row(
            "SELECT dag FROM workflows WHERE id = ?1",
            params![workflow_id],
            |row| row.get(0),
        )
        .map_err(|e| anyhow!("Failed to load workflow {}: {}", workflow_id, e))?;

    Ok((workflow_id, tenant_id, dag_json))
}

/// Mark a run as `running` and stamp `started_at` (only if not already set).
fn set_run_running(
    db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    run_id: &str,
) -> Result<()> {
    let conn = db.get().map_err(|e| anyhow!("DB pool error: {}", e))?;
    conn.execute(
        "UPDATE workflow_runs
         SET status = 'running',
             started_at = COALESCE(started_at, datetime('now')),
             current_steps = COALESCE(current_steps, '[]'),
             completed_steps = COALESCE(completed_steps, '[]')
         WHERE id = ?1",
        params![run_id],
    )
    .map_err(|e| anyhow!("Failed to mark run {} as running: {}", run_id, e))?;
    Ok(())
}

/// Set the terminal status + `finished_at` timestamp on a run.
fn finalize_run(
    db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    run_id: &str,
    status: &str,
) -> Result<()> {
    let conn = db.get().map_err(|e| anyhow!("DB pool error: {}", e))?;
    conn.execute(
        "UPDATE workflow_runs
         SET status = ?1,
             finished_at = datetime('now'),
             current_steps = '[]'
         WHERE id = ?2",
        params![status, run_id],
    )
    .map_err(|e| anyhow!("Failed to finalize run {}: {}", run_id, e))?;
    Ok(())
}

/// Persist the current and completed step arrays for a run.
///
/// `current` is the set of step IDs currently being executed (emptied once
/// the batch finishes). `completed` is the full set of finished / skipped
/// steps so far. Both are stored as JSON arrays.
fn update_step_arrays(
    db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    run_id: &str,
    current: &[&str],
    completed: &HashSet<String>,
) -> Result<()> {
    let current_json = serde_json::to_string(current)?;
    let completed_vec: Vec<&str> = completed.iter().map(|s| s.as_str()).collect();
    let completed_json = serde_json::to_string(&completed_vec)?;

    let conn = db.get().map_err(|e| anyhow!("DB pool error: {}", e))?;
    conn.execute(
        "UPDATE workflow_runs
         SET current_steps = ?1, completed_steps = ?2
         WHERE id = ?3",
        params![current_json, completed_json, run_id],
    )
    .map_err(|e| anyhow!("Failed to update step arrays for run {}: {}", run_id, e))?;
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ─── DAG deserialization ─────────────────────────────────────────────

    #[test]
    fn test_dag_deserialize_minimal() {
        let json = r#"{
            "steps": [
                {"id": "a", "task": "echo a"}
            ]
        }"#;
        let dag: Dag = serde_json::from_str(json).unwrap();
        assert_eq!(dag.steps.len(), 1);
        assert_eq!(dag.steps[0].id, "a");
        assert_eq!(dag.steps[0].task, "echo a");
        assert!(dag.steps[0].depends_on.is_empty());
        assert!(dag.steps[0].condition.is_none());
        assert!(dag.steps[0].max_retries.is_none());
    }

    #[test]
    fn test_dag_deserialize_full_step() {
        let json = r#"{
            "steps": [{
                "id": "build",
                "task": "cargo build --release",
                "image": "stronghold/rust-nightly:latest",
                "ttl_secs": 1800,
                "context": {"env": {"CI": "true"}},
                "depends_on": ["setup"],
                "condition": "setup.result.exit_code == 0",
                "max_retries": 5
            }]
        }"#;
        let dag: Dag = serde_json::from_str(json).unwrap();
        let s = &dag.steps[0];
        assert_eq!(s.id, "build");
        assert_eq!(s.image.as_deref(), Some("stronghold/rust-nightly:latest"));
        assert_eq!(s.ttl_secs, Some(1800));
        assert_eq!(s.depends_on, vec!["setup"]);
        assert_eq!(s.condition.as_deref(), Some("setup.result.exit_code == 0"));
        assert_eq!(s.max_retries, Some(5));
    }

    #[test]
    fn test_dag_deserialize_empty_steps() {
        let json = r#"{ "steps": [] }"#;
        let dag: Dag = serde_json::from_str(json).unwrap();
        assert!(dag.steps.is_empty());
    }

    #[test]
    fn test_step_effective_defaults() {
        let s = serde_json::from_str::<Step>(
            r#"{"id":"x","task":"echo"}"#,
        )
        .unwrap();
        assert_eq!(s.effective_max_retries(), 3);
        assert_eq!(s.effective_image(), "stronghold/rocky-base:latest");
        assert_eq!(s.effective_ttl(), 3600);
    }

    // ─── Dependency resolution ───────────────────────────────────────────

    #[test]
    fn test_find_ready_steps_no_deps() {
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "a", "task": "a"},
                {"id": "b", "task": "b"}
            ]
        }"#).unwrap();
        let completed = HashSet::new();
        let ready = find_ready_steps(&dag.steps, &completed);
        let mut ids: Vec<&str> = ready.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn test_find_ready_steps_with_unmet_deps() {
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "a", "task": "a"},
                {"id": "b", "task": "b", "depends_on": ["a"]},
                {"id": "c", "task": "c", "depends_on": ["b"]}
            ]
        }"#).unwrap();
        let completed = HashSet::new();
        let ready = find_ready_steps(&dag.steps, &completed);
        let ids: Vec<&str> = ready.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn test_find_ready_steps_with_met_deps() {
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "a", "task": "a"},
                {"id": "b", "task": "b", "depends_on": ["a"]},
                {"id": "c", "task": "c", "depends_on": ["a"]}
            ]
        }"#).unwrap();
        let completed: HashSet<String> = ["a".to_string()].into_iter().collect();
        let ready = find_ready_steps(&dag.steps, &completed);
        let mut ids: Vec<&str> = ready.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["b", "c"]);
    }

    #[test]
    fn test_find_ready_steps_excludes_completed() {
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "a", "task": "a"},
                {"id": "b", "task": "b"}
            ]
        }"#).unwrap();
        let completed: HashSet<String> = ["a".to_string()].into_iter().collect();
        let ready = find_ready_steps(&dag.steps, &completed);
        let ids: Vec<&str> = ready.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn test_find_ready_steps_diamond() {
        // Diamond: a → b, a → c, b+c → d
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "a", "task": "a"},
                {"id": "b", "task": "b", "depends_on": ["a"]},
                {"id": "c", "task": "c", "depends_on": ["a"]},
                {"id": "d", "task": "d", "depends_on": ["b", "c"]}
            ]
        }"#).unwrap();

        // Wave 1: only a
        let mut completed = HashSet::new();
        let wave1 = find_ready_steps(&dag.steps, &completed);
        assert_eq!(ids_of(&wave1), vec!["a"]);
        completed.insert("a".to_string());

        // Wave 2: b and c (both depend only on a)
        let wave2 = find_ready_steps(&dag.steps, &completed);
        let mut w2 = ids_of(&wave2);
        w2.sort();
        assert_eq!(w2, vec!["b", "c"]);
        completed.insert("b".to_string());
        completed.insert("c".to_string());

        // Wave 3: d (depends on b and c)
        let wave3 = find_ready_steps(&dag.steps, &completed);
        assert_eq!(ids_of(&wave3), vec!["d"]);
    }

    // ─── Condition evaluation ────────────────────────────────────────────

    #[test]
    fn test_evaluate_condition_empty_is_true() {
        let results = HashMap::new();
        assert!(evaluate_condition("", &results));
        assert!(evaluate_condition("   ", &results));
    }

    #[test]
    fn test_evaluate_condition_eq_exit_code_zero() {
        let mut results = HashMap::new();
        results.insert(
            "build".to_string(),
            serde_json::json!({"result": {"exit_code": 0, "stdout": "ok"}}),
        );
        assert!(evaluate_condition("build.result.exit_code == 0", &results));
        assert!(!evaluate_condition("build.result.exit_code == 1", &results));
    }

    #[test]
    fn test_evaluate_condition_neq_exit_code() {
        let mut results = HashMap::new();
        results.insert(
            "build".to_string(),
            serde_json::json!({"result": {"exit_code": 2}}),
        );
        assert!(evaluate_condition("build.result.exit_code != 0", &results));
        assert!(!evaluate_condition("build.result.exit_code != 2", &results));
    }

    #[test]
    fn test_evaluate_condition_missing_step_is_false() {
        let results = HashMap::new();
        // Step "x" never ran → path can't resolve → condition false (fail safe).
        assert!(!evaluate_condition("x.result.exit_code == 0", &results));
    }

    #[test]
    fn test_evaluate_condition_missing_field_is_false() {
        let mut results = HashMap::new();
        results.insert(
            "build".to_string(),
            serde_json::json!({"result": {"exit_code": 0}}),
        );
        // "stdout" exists, "missing" doesn't.
        assert!(!evaluate_condition("build.result.missing == 0", &results));
    }

    #[test]
    fn test_evaluate_condition_string_value() {
        let mut results = HashMap::new();
        results.insert(
            "check".to_string(),
            serde_json::json!({"result": {"summary": "passed"}}),
        );
        assert!(evaluate_condition("check.result.summary == \"passed\"", &results));
        assert!(!evaluate_condition("check.result.summary == \"failed\"", &results));
    }

    #[test]
    fn test_evaluate_condition_boolean_value() {
        let mut results = HashMap::new();
        results.insert(
            "gate".to_string(),
            serde_json::json!({"result": {"ok": true}}),
        );
        assert!(evaluate_condition("gate.result.ok == true", &results));
        assert!(!evaluate_condition("gate.result.ok == false", &results));
    }

    #[test]
    fn test_evaluate_condition_unparseable_is_false() {
        let results = HashMap::new();
        // No operator → unparseable → fail safe.
        assert!(!evaluate_condition("just_a_path", &results));
        assert!(!evaluate_condition("build.result.exit_code", &results));
    }

    #[test]
    fn test_value_equals_numbers() {
        assert!(value_equals(&serde_json::json!(0), "0"));
        assert!(value_equals(&serde_json::json!(42), "42"));
        assert!(!value_equals(&serde_json::json!(42), "41"));
        // Float comparison.
        assert!(value_equals(&serde_json::json!(3.14), "3.14"));
    }

    #[test]
    fn test_value_equals_strings() {
        assert!(value_equals(&serde_json::json!("ok"), "ok"));
        assert!(value_equals(&serde_json::json!("ok"), "\"ok\""));
        assert!(!value_equals(&serde_json::json!("ok"), "fail"));
    }

    #[test]
    fn test_value_equals_booleans() {
        assert!(value_equals(&serde_json::json!(true), "true"));
        assert!(value_equals(&serde_json::json!(false), "false"));
        assert!(!value_equals(&serde_json::json!(true), "false"));
    }

    #[test]
    fn test_value_equals_null() {
        assert!(value_equals(&serde_json::Value::Null, "null"));
        assert!(!value_equals(&serde_json::Value::Null, "0"));
    }

    // ─── Path resolution ─────────────────────────────────────────────────

    #[test]
    fn test_resolve_path_simple() {
        let mut results = HashMap::new();
        results.insert("a".to_string(), serde_json::json!({"result": {"exit_code": 0}}));
        let v = resolve_path("a.result.exit_code", &results).unwrap();
        assert_eq!(v, &serde_json::json!(0));
    }

    #[test]
    fn test_resolve_path_missing_root() {
        let results = HashMap::new();
        assert!(resolve_path("x.result.exit_code", &results).is_none());
    }

    #[test]
    fn test_resolve_path_missing_segment() {
        let mut results = HashMap::new();
        results.insert("a".to_string(), serde_json::json!({"result": {}}));
        assert!(resolve_path("a.result.exit_code", &results).is_none());
    }

    // ─── Dependency met checks ───────────────────────────────────────────

    #[test]
    fn test_dependencies_met_empty() {
        let step = serde_json::from_str::<Step>(r#"{"id":"a","task":"a"}"#).unwrap();
        let completed = HashSet::new();
        assert!(dependencies_met(&step, &completed));
    }

    #[test]
    fn test_dependencies_met_all_present() {
        let step = serde_json::from_str::<Step>(
            r#"{"id":"c","task":"c","depends_on":["a","b"]}"#,
        )
        .unwrap();
        let completed: HashSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        assert!(dependencies_met(&step, &completed));
    }

    #[test]
    fn test_dependencies_met_some_missing() {
        let step = serde_json::from_str::<Step>(
            r#"{"id":"c","task":"c","depends_on":["a","b"]}"#,
        )
        .unwrap();
        let completed: HashSet<String> = ["a".to_string()].into_iter().collect();
        assert!(!dependencies_met(&step, &completed));
    }

    // ─── Topological ordering (wave simulation) ──────────────────────────

    /// Helper: extract step IDs from a slice of &Step in order.
    fn ids_of<'a>(steps: &[&'a Step]) -> Vec<&'a str> {
        steps.iter().map(|s| s.id.as_str()).collect()
    }

    /// Simulate the DAG executor's wave-by-wave scheduling against a mock DAG
    /// and verify that the execution order respects dependencies.
    ///
    /// This mirrors the loop in `execute_dag` but without a database: each
    /// wave is the set of ready steps given the current `completed` set, and
    /// all steps in a wave are "completed" before the next wave is computed.
    #[test]
    fn test_topological_order_linear_chain() {
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "a", "task": "a"},
                {"id": "b", "task": "b", "depends_on": ["a"]},
                {"id": "c", "task": "c", "depends_on": ["b"]}
            ]
        }"#).unwrap();

        let mut completed = HashSet::new();
        let mut order: Vec<Vec<String>> = Vec::new();

        while completed.len() < dag.steps.len() {
            let wave = find_ready_steps(&dag.steps, &completed);
            if wave.is_empty() {
                panic!("DAG stuck before completion");
            }
            let wave_ids: Vec<String> = wave.iter().map(|s| s.id.clone()).collect();
            for id in &wave_ids {
                completed.insert(id.clone());
            }
            order.push(wave_ids);
        }

        // Linear chain → each wave has exactly one step.
        assert_eq!(order.len(), 3);
        assert_eq!(order[0], vec!["a"]);
        assert_eq!(order[1], vec!["b"]);
        assert_eq!(order[2], vec!["c"]);
    }

    #[test]
    fn test_topological_order_parallel_branches() {
        // a → {b, c} → d   (b and c run in parallel)
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "a", "task": "a"},
                {"id": "b", "task": "b", "depends_on": ["a"]},
                {"id": "c", "task": "c", "depends_on": ["a"]},
                {"id": "d", "task": "d", "depends_on": ["b", "c"]}
            ]
        }"#).unwrap();

        let mut completed = HashSet::new();
        let mut order: Vec<Vec<String>> = Vec::new();

        while completed.len() < dag.steps.len() {
            let wave = find_ready_steps(&dag.steps, &completed);
            if wave.is_empty() {
                panic!("DAG stuck before completion");
            }
            let mut wave_ids: Vec<String> =
                wave.iter().map(|s| s.id.clone()).collect();
            wave_ids.sort();
            for id in &wave_ids {
                completed.insert(id.clone());
            }
            order.push(wave_ids);
        }

        assert_eq!(order.len(), 3);
        assert_eq!(order[0], vec!["a"]);
        assert_eq!(order[1], vec!["b", "c"]);
        assert_eq!(order[2], vec!["d"]);
    }

    #[test]
    fn test_topological_order_respects_dependencies() {
        // Build a non-trivial DAG and verify that every step appears after
        // all its dependencies.
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "fetch",  "task": "f"},
                {"id": "lint",   "task": "l", "depends_on": ["fetch"]},
                {"id": "build",  "task": "b", "depends_on": ["fetch"]},
                {"id": "test",   "task": "t", "depends_on": ["build"]},
                {"id": "pack",   "task": "p", "depends_on": ["build", "lint"]},
                {"id": "publish","task": "P", "depends_on": ["test", "pack"]}
            ]
        }"#).unwrap();

        let mut completed = HashSet::new();
        let mut position: HashMap<String, usize> = HashMap::new();
        let mut wave_idx = 0;

        while completed.len() < dag.steps.len() {
            let wave = find_ready_steps(&dag.steps, &completed);
            if wave.is_empty() {
                panic!("DAG stuck before completion");
            }
            for s in &wave {
                position.insert(s.id.clone(), wave_idx);
                completed.insert(s.id.clone());
            }
            wave_idx += 1;
        }

        // Verify: every step's wave > all its dependencies' waves.
        for step in &dag.steps {
            let my_wave = position[&step.id];
            for dep in &step.depends_on {
                let dep_wave = position[dep];
                assert!(
                    my_wave > dep_wave,
                    "step {} (wave {}) must run after its dep {} (wave {})",
                    step.id,
                    my_wave,
                    dep,
                    dep_wave
                );
            }
        }
    }

    #[test]
    fn test_dag_cycle_is_stuck() {
        // a → b → a  (cycle)
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "a", "task": "a", "depends_on": ["b"]},
                {"id": "b", "task": "b", "depends_on": ["a"]}
            ]
        }"#).unwrap();

        let completed = HashSet::new();
        let ready = find_ready_steps(&dag.steps, &completed);
        // Neither a nor b is ready (both have unmet deps) → executor would
        // detect this as "stuck" and mark the run as failed.
        assert!(ready.is_empty(), "cyclic DAG should have no ready steps");
    }

    // ─── Condition-driven scheduling simulation ──────────────────────────

    /// Simulate the partition-by-condition logic from `execute_dag`.
    ///
    /// Steps with an unmet condition are skipped (added to `completed` with
    /// a placeholder result) so downstream steps can proceed — mirroring the
    /// executor's behaviour without requiring a database.
    #[test]
    fn test_condition_skips_step_and_allows_downstream() {
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "check", "task": "test -f file"},
                {"id": "build", "task": "make", "depends_on": ["check"],
                 "condition": "check.result.exit_code == 0"},
                {"id": "notify", "task": "echo done", "depends_on": ["build"]}
            ]
        }"#).unwrap();

        let mut completed = HashSet::new();
        let mut results = HashMap::new();

        // Wave 1: check runs.
        let wave1 = find_ready_steps(&dag.steps, &completed);
        assert_eq!(ids_of(&wave1), vec!["check"]);
        // Simulate check "failing" (exit_code 1 — file not found).
        results.insert(
            "check".to_string(),
            serde_json::json!({"result": {"exit_code": 1}}),
        );
        completed.insert("check".to_string());

        // Wave 2: build's deps are met, but its condition fails → skipped.
        let wave2 = find_ready_steps(&dag.steps, &completed);
        assert_eq!(ids_of(&wave2), vec!["build"]);
        let cond = wave2[0].condition.as_deref().unwrap_or("");
        assert!(!evaluate_condition(cond, &results), "condition should be false");
        // Simulate skip: add placeholder result.
        results.insert(
            "build".to_string(),
            serde_json::json!({"result": {"skipped": true}}),
        );
        completed.insert("build".to_string());

        // Wave 3: notify's deps (build) are met → it runs.
        let wave3 = find_ready_steps(&dag.steps, &completed);
        assert_eq!(ids_of(&wave3), vec!["notify"]);
    }

    #[test]
    fn test_condition_met_allows_step() {
        let dag: Dag = serde_json::from_str(r#"{
            "steps": [
                {"id": "check", "task": "test -f file"},
                {"id": "build", "task": "make", "depends_on": ["check"],
                 "condition": "check.result.exit_code == 0"}
            ]
        }"#).unwrap();

        let mut completed = HashSet::new();
        let mut results = HashMap::new();

        // Wave 1: check succeeds (exit_code 0).
        let wave1 = find_ready_steps(&dag.steps, &completed);
        assert_eq!(ids_of(&wave1), vec!["check"]);
        results.insert(
            "check".to_string(),
            serde_json::json!({"result": {"exit_code": 0}}),
        );
        completed.insert("check".to_string());

        // Wave 2: build's condition is met → it would run.
        let wave2 = find_ready_steps(&dag.steps, &completed);
        assert_eq!(ids_of(&wave2), vec!["build"]);
        let cond = wave2[0].condition.as_deref().unwrap_or("");
        assert!(evaluate_condition(cond, &results), "condition should be true");
    }
}
