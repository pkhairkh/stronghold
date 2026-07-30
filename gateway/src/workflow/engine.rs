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
//!    c. Spawn one [`tokio::task`] per runnable step. Each task calls
//!       [`crate::workflow::executor::execute_step`], which schedules a fresh
//!       `wf-*` pod, waits for `Ready`, runs `sh -c "<task>"` via `kube exec`,
//!       captures stdout/stderr/exit_code, and kills the pod.
//!    d. On `completed`: store the result, add to `completed_steps`.
//!       On `failed` (after `max_retries`): store the result, mark the run
//!       `failed` and stop.
//!    e. Update `workflow_runs.current_steps` / `completed_steps` /
//!       `step_results` after each batch.
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
/// Thin spawn-friendly wrapper around [`advance_dag`]: takes ownership of
/// an [`AppState`] (so it can be `tokio::spawn`'d by the route handler)
/// and delegates the actual work to `advance_dag(&state, run_id)`.
///
/// # Errors
/// Returns an error if:
/// - the run or workflow can't be loaded,
/// - a step fails after exhausting its retries,
/// - the DAG has a cycle (no ready steps, not all completed),
/// - a database error occurs.
pub async fn execute(run_id: &str, state: AppState) -> Result<()> {
    advance_dag(&state, run_id).await
}

/// Drive a workflow run to completion (or failure).
///
/// This is the **V2** DAG advancement entry point. It:
/// 1. Loads the run + workflow + DAG from the database.
/// 2. Marks the run `running`.
/// 3. Walks the DAG via [`execute_dag`]: finds ready steps, partitions by
///    `condition`, launches the runnable ones concurrently via
///    [`tokio::spawn`] (each calling [`crate::workflow::executor::execute_step`]),
///    retries failures up to `max_retries`, evaluates downstream conditions,
///    and persists `current_steps` / `completed_steps` / `step_results`
///    after each wave.
/// 4. Finalizes the run status (`completed` or `failed`) + `finished_at`.
/// 5. Writes a `workflow_completed` / `workflow_failed` audit entry.
///
/// Step results are persisted to `workflow_runs.step_results` as a JSON map
/// `{step_id: {exit_code, stdout, stderr, duration_ms}}` after each wave.
///
/// # Errors
/// - `Failed to load workflow run …` — run_id doesn't exist.
/// - `Invalid DAG JSON …` — workflow.dag is malformed.
/// - `Workflow stuck: … of … steps completed, none ready` — DAG has a cycle
///   or a step depends on a step that was skipped/failed and never made it
///   into `completed`.
/// - `One or more steps failed` — a step failed after exhausting retries.
pub async fn advance_dag(state: &AppState, run_id: &str) -> Result<()> {
    tracing::info!(run_id = run_id, "Workflow run starting");

    // 1. Load the run + workflow + DAG.
    let (workflow_id, tenant_id, dag_json) = load_run(&state.db, run_id)?;
    let dag: Dag = serde_json::from_str(&dag_json)
        .map_err(|e| anyhow!("Invalid DAG JSON for workflow {}: {}", workflow_id, e))?;

    // 2. Mark the run as running (with empty step arrays if not already set).
    set_run_running(&state.db, run_id)?;

    // 3. Walk the DAG.
    let outcome = execute_dag(run_id, &tenant_id, &dag, state).await;

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
            update_step_results(&state.db, run_id, &results)?;
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

        // Persist the updated completed_steps + step_results.
        update_step_arrays(&state.db, run_id, &[], &completed)?;
        update_step_results(&state.db, run_id, &results)?;

        if batch_failed {
            return Err(anyhow!("One or more steps failed"));
        }
    }

    Ok(())
}

// ============================================================================
// Step execution (with retries)
// ============================================================================

/// Run a step, retrying on failure up to `max_retries` times.
///
/// Each attempt calls [`crate::workflow::executor::execute_step`], which
/// schedules a fresh `wf-*` pod, runs `sh -c "<task>"` via `kube exec`,
/// captures stdout/stderr/exit_code, and tears the pod down. Returns the
/// [`StepResult`](crate::workflow::executor::StepResult) of the successful
/// attempt, wrapped as `{"result": <StepResult>}` so the condition
/// evaluator can look up `<step>.result.exit_code`.
///
/// `tenant_id` is kept in the signature for caller symmetry but unused —
/// `execute_step` looks up the tenant itself from `workflow_runs.tenant_id`.
async fn run_step_with_retries(
    state: &AppState,
    run_id: &str,
    _tenant_id: &str,
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
        match crate::workflow::executor::execute_step(state, run_id, step).await {
            Ok(result) => {
                return Ok(serde_json::json!({ "result": result }));
            }
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

/// Persist the per-step results map to `workflow_runs.step_results`.
///
/// The in-memory `results` map stores each step's outcome as
/// `{"result": <StepResult>}` (with the `result` wrapper so the condition
/// evaluator can look up `<step>.result.exit_code`). The `step_results`
/// column stores the unwrapped map `{step_id: <StepResult>}` — i.e. the
/// shape callers poll via `GET /workflow/run/:id`.
///
/// Skipped steps appear as `{"skipped": true}` (the inner value of their
/// `{"result": {"skipped": true}}` placeholder).
fn update_step_results(
    db: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    run_id: &str,
    results: &HashMap<String, serde_json::Value>,
) -> Result<()> {
    // Strip the "result" wrapper to produce {step_id: {exit_code, stdout, ...}}.
    let mut map = serde_json::Map::new();
    for (k, v) in results.iter() {
        if let Some(inner) = v.get("result") {
            map.insert(k.clone(), inner.clone());
        }
    }
    let json = serde_json::Value::Object(map).to_string();

    let conn = db.get().map_err(|e| anyhow!("DB pool error: {}", e))?;
    conn.execute(
        "UPDATE workflow_runs SET step_results = ?1 WHERE id = ?2",
        params![json, run_id],
    )
    .map_err(|e| anyhow!("Failed to update step_results for run {}: {}", run_id, e))?;
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

    // ─── step_results persistence ───────────────────────────────────────

    /// `update_step_results` must strip the `result` wrapper from each
    /// entry, producing a JSON map `{step_id: <StepResult>}` that callers
    /// can poll via `GET /workflow/run/:id`.
    #[test]
    fn test_update_step_results_strips_wrapper() {
        let pool = crate::db::init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        // Insert a tenant + workflow + run so the UPDATE has a target row.
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('t1', 'T', datetime('now'), 'x', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workflows (id, tenant_id, name, dag, status, created_at)
             VALUES ('wf1', 't1', 'W', '{\"steps\":[]}', 'active', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workflow_runs (id, workflow_id, tenant_id, status, current_steps, completed_steps, started_at)
             VALUES ('r1', 'wf1', 't1', 'running', '[]', '[]', datetime('now'))",
            [],
        )
        .unwrap();
        drop(conn);

        // Two completed steps + one skipped step.
        let mut results: HashMap<String, serde_json::Value> = HashMap::new();
        results.insert(
            "s1".to_string(),
            serde_json::json!({"result": {"exit_code": 0, "stdout": "hi\n", "stderr": "", "duration_ms": 12}}),
        );
        results.insert(
            "s2".to_string(),
            serde_json::json!({"result": {"exit_code": 1, "stdout": "", "stderr": "boom", "duration_ms": 34}}),
        );
        results.insert(
            "s3".to_string(),
            serde_json::json!({"result": {"skipped": true}}),
        );

        update_step_results(&pool, "r1", &results).unwrap();

        let stored: String = pool.get().unwrap()
            .query_row(
                "SELECT step_results FROM workflow_runs WHERE id = 'r1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        // Wrapper is stripped — keys map directly to the inner StepResult.
        assert_eq!(parsed["s1"]["exit_code"], 0);
        assert_eq!(parsed["s1"]["stdout"], "hi\n");
        assert_eq!(parsed["s2"]["exit_code"], 1);
        assert_eq!(parsed["s2"]["stderr"], "boom");
        assert_eq!(parsed["s3"]["skipped"], true);
        // No top-level "result" key.
        assert!(parsed.get("result").is_none());
    }

    /// `update_step_results` must produce `{}` (not `null`) when the results
    /// map is empty — so callers can always `serde_json::from_str` the column.
    #[test]
    fn test_update_step_results_empty_map() {
        let pool = crate::db::init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('t2', 'T', datetime('now'), 'x', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workflows (id, tenant_id, name, dag, status, created_at)
             VALUES ('wf2', 't2', 'W', '{\"steps\":[]}', 'active', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workflow_runs (id, workflow_id, tenant_id, status, current_steps, completed_steps, started_at)
             VALUES ('r2', 'wf2', 't2', 'running', '[]', '[]', datetime('now'))",
            [],
        )
        .unwrap();
        drop(conn);

        let results: HashMap<String, serde_json::Value> = HashMap::new();
        update_step_results(&pool, "r2", &results).unwrap();

        let stored: String = pool.get().unwrap()
            .query_row(
                "SELECT step_results FROM workflow_runs WHERE id = 'r2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "{}");
    }

    // ─── advance_dag error paths (no k8s required) ──────────────────────

    /// `advance_dag` on a nonexistent run_id must error out at `load_run`
    /// before ever touching the k8s API.
    #[tokio::test]
    async fn test_advance_dag_unknown_run_errors() {
        let pool = crate::db::init_memory_pool().unwrap();
        let keys = crate::crypto::hybrid_sig::AuditKeys::generate();
        let push_keys = crate::crypto::hybrid_kem::PushKeys::generate();
        let state = AppState {
            db: pool,
            audit_keys: keys,
            push_keys,
            pty_registry: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        };
        let err = advance_dag(&state, "nonexistent_run").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Failed to load workflow run"),
            "expected load_run error, got: {}",
            msg
        );
    }

    // ─── k3s integration (manual; #[ignore] by default) ─────────────────
    //
    // These tests run real `wf-*` pods on the dev box's k3s cluster. They
    // are `#[ignore]`'d so they don't run under `cargo test workflow` (which
    // the DoD runs in CI). Run manually with:
    //
    //   KUBECONFIG=/etc/rancher/k3s/k3s.yaml \
    //     cargo test --features no-sev-snp workflow::engine -- --ignored
    //
    // Each test skips gracefully if the kube client can't be created.

    /// Build an AppState with a tenant + workflow (dag passed in) + run row
    /// `r1` ready for `advance_dag`.
    fn integration_state(dag_json: &str) -> AppState {
        let pool = crate::db::init_memory_pool().unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO tenants (id, name, created_at, setup_password, setup_used)
             VALUES ('t_int', 'T', datetime('now'), 'x', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workflows (id, tenant_id, name, dag, status, created_at)
             VALUES ('wf_int', 't_int', 'W', ?1, 'active', datetime('now'))",
            rusqlite::params![dag_json],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workflow_runs
             (id, workflow_id, tenant_id, status, current_steps, completed_steps, started_at)
             VALUES ('r1', 'wf_int', 't_int', 'running', '[]', '[]', datetime('now'))",
            [],
        )
        .unwrap();
        drop(conn);

        let keys = crate::crypto::hybrid_sig::AuditKeys::generate();
        let push_keys = crate::crypto::hybrid_kem::PushKeys::generate();
        AppState {
            db: pool,
            audit_keys: keys,
            push_keys,
            pty_registry: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    async fn skip_if_no_k8s() -> bool {
        // Install the rustls CryptoProvider (main.rs does this in prod, but
        // test binaries don't run main.rs). Idempotent via `Once`.
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
        match crate::machines::scheduler::list_pods().await {
            Ok(_) => false,
            Err(e) => {
                eprintln!("skipping k3s integration test: {}", e);
                true
            }
        }
    }

    /// Read the run's terminal status + step_results JSON for assertions.
    fn read_run_outcome(
        state: &AppState,
    ) -> (String, serde_json::Value) {
        let conn = state.db.get().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM workflow_runs WHERE id = 'r1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let step_results_str: String = conn
            .query_row(
                "SELECT COALESCE(step_results, '{}') FROM workflow_runs WHERE id = 'r1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let step_results: serde_json::Value =
            serde_json::from_str(&step_results_str).unwrap();
        (status, step_results)
    }

    /// DoD: a 2-step linear DAG (s1 → s2) executes both steps in order.
    #[tokio::test]
    #[ignore]
    async fn integration_linear_dag_runs_both_steps() {
        if skip_if_no_k8s().await {
            return;
        }
        // s1 writes "step1" to a file; s2 cats it and prints "step2".
        // Both must run; s2 depends on s1.
        let dag = r#"{
            "steps": [
                {"id": "s1", "task": "echo step1"},
                {"id": "s2", "task": "echo step2", "depends_on": ["s1"]}
            ]
        }"#;
        let state = integration_state(dag);
        advance_dag(&state, "r1").await.unwrap();
        let (status, results) = read_run_outcome(&state);
        assert_eq!(status, "completed");
        assert_eq!(results["s1"]["exit_code"], 0);
        assert_eq!(results["s1"]["stdout"].as_str().unwrap().trim(), "step1");
        assert_eq!(results["s2"]["exit_code"], 0);
        assert_eq!(results["s2"]["stdout"].as_str().unwrap().trim(), "step2");

        // No pod leaked (poll — kill_pod returns before the pod is fully reaped).
        assert_no_workflow_pods_leaked().await;
    }

    /// DoD: a 2-step parallel DAG (s1, s2 independent) executes both.
    ///
    /// We can't directly assert concurrency from outside the engine, but we
    /// verify both steps ran (both appear in step_results with exit_code 0)
    /// and the run completed successfully.
    #[tokio::test]
    #[ignore]
    async fn integration_parallel_dag_runs_both_steps() {
        if skip_if_no_k8s().await {
            return;
        }
        let dag = r#"{
            "steps": [
                {"id": "s1", "task": "echo a"},
                {"id": "s2", "task": "echo b"}
            ]
        }"#;
        let state = integration_state(dag);
        advance_dag(&state, "r1").await.unwrap();
        let (status, results) = read_run_outcome(&state);
        assert_eq!(status, "completed");
        assert_eq!(results["s1"]["exit_code"], 0);
        assert_eq!(results["s2"]["exit_code"], 0);
    }

    /// DoD: a conditional DAG skips s2 if s1 "fails" (returns non-zero).
    ///
    /// s1 runs `exit 1` → completed with exit_code 1.
    /// s2's condition `s1.result.exit_code == 0` is false → s2 is skipped.
    /// Run status is "completed" because skipped steps count as completed.
    #[tokio::test]
    #[ignore]
    async fn integration_conditional_dag_skips_on_failure() {
        if skip_if_no_k8s().await {
            return;
        }
        let dag = r#"{
            "steps": [
                {"id": "s1", "task": "exit 1", "max_retries": 0},
                {"id": "s2", "task": "echo should_not_run",
                 "depends_on": ["s1"], "condition": "s1.result.exit_code == 0"}
            ]
        }"#;
        let state = integration_state(dag);
        advance_dag(&state, "r1").await.unwrap();
        let (status, results) = read_run_outcome(&state);
        assert_eq!(status, "completed");
        // s1 ran and returned non-zero — but the step itself succeeded.
        assert_eq!(results["s1"]["exit_code"], 1);
        // s2 was skipped (condition false).
        assert_eq!(results["s2"]["skipped"], true);
        // The skipped step did NOT actually run its task — verify by checking
        // there's no exit_code field on the s2 entry.
        assert!(
            results["s2"].get("exit_code").is_none(),
            "skipped step should not have an exit_code; got: {}",
            results["s2"]
        );
    }

    /// DoD: run status transitions to `failed` when a step errors (not just
    /// returns non-zero — actually errors, e.g. the task spec is invalid).
    ///
    /// We force an error by giving the step an image that doesn't exist; the
    /// scheduler will create the pod but it will never reach Ready (ErrImagePull /
    /// ImagePullBackOff), and `wait_for_pod_ready` will time out → `execute_step`
    /// returns Err → `run_step_with_retries` exhausts retries → `execute_dag`
    /// returns Err → `advance_dag` finalizes the run as `failed`.
    #[tokio::test]
    #[ignore]
    async fn integration_failed_step_marks_run_failed() {
        if skip_if_no_k8s().await {
            return;
        }
        // Use a non-existent image — k8s will fail to pull it.
        // The readiness timeout in executor.rs is 120s; with max_retries=0
        // the total test time is ~120s. Marked #[ignore] so it only runs
        // when explicitly requested.
        let dag = r#"{
            "steps": [
                {"id": "s1", "task": "echo hello",
                 "image": "localhost:30500/stronghold/does-not-exist:latest",
                 "max_retries": 0}
            ]
        }"#;
        let state = integration_state(dag);
        // advance_dag returns Err, but still finalizes the run as "failed".
        let _ = advance_dag(&state, "r1").await;
        let (status, _results) = read_run_outcome(&state);
        assert_eq!(
            status, "failed",
            "run with a failing step should be marked failed"
        );

        // No pod leaked even on failure (poll — kill_pod returns before the
        // pod is fully reaped).
        assert_no_workflow_pods_leaked().await;
    }

    /// Poll `list_pods` for up to ~15s waiting for all `wf-*` pods to
    /// disappear. `kill_pod` returns as soon as deletion is initiated, but
    /// the pod may remain visible (in `Terminating` state) for a few seconds
    /// while k8s actually reaps it.
    async fn assert_no_workflow_pods_leaked() {
        for _ in 0..30 {
            let pods = crate::machines::scheduler::list_pods().await.unwrap();
            let leaked: Vec<_> = pods.iter().filter(|p| p.starts_with("wf-")).collect();
            if leaked.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let pods = crate::machines::scheduler::list_pods().await.unwrap();
        let leaked: Vec<_> = pods.iter().filter(|p| p.starts_with("wf-")).collect();
        assert!(leaked.is_empty(), "workflow pods leaked: {:?}", leaked);
    }
}
