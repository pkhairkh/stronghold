//! T2: Structural validation of all 12 workflow templates in
//! `agent/templates/`.
//!
//! Each template is a JSON file of the form:
//! ```json
//! {
//!   "name": "standard-cicd",
//!   "dag": {
//!     "steps": [
//!       {
//!         "id": "plan",
//!         "task": {
//!           "instruction": "Analyze the issue...",
//!           "image": "stronghold/rust-nightly",
//!           "ttl_secs": 1800
//!         },
//!         "depends_on": [],
//!         "role": "planner",            // optional
//!         "condition": "prev.result.exit_code == 0"  // optional
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! This test reads every `*.json` file under `agent/templates/` and validates:
//!
//! 1. **Valid JSON** — parses with `serde_json`.
//! 2. **Top-level shape** — has a non-empty `name` (string) and a `dag`
//!    object containing a `steps` array.
//! 3. **Per-step fields** — every step has an `id` (string) and a `task`
//!    object with non-empty `instruction` (string), non-empty `image`
//!    (string), and a positive integer `ttl_secs`.
//! 4. **No dangling `depends_on`** — every entry in each step's `depends_on`
//!    array references a step ID that exists in the same DAG.
//! 5. **No circular dependencies** — a topological sort (Kahn's algorithm)
//!    consumes every step. If any step remains unprocessed, the DAG has a
//!    cycle.
//! 6. **No dangling `condition` / `parallel_with` references** — the step ID
//!    in the first segment of any `condition` string, and any
//!    `parallel_with` value, must reference an existing step in the same
//!    DAG.
//!
//! The test asserts that **all 12** templates pass every check. The expected
//! template count is hard-coded so adding a 13th template without updating
//! this test produces a visible failure.
//!
//! Run with:
//!     cargo test --workspace --features no-sev-snp --test template_test

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

/// The expected number of template files. Update this when adding a new
/// template (and add the new template to the `EXPECTED_TEMPLATE_NAMES` set).
const EXPECTED_TEMPLATE_COUNT: usize = 12;

/// The 12 expected template names (file stems). Guards against accidental
/// deletion and ensures the test surfaces additions.
const EXPECTED_TEMPLATE_NAMES: &[&str] = &[
    "bug-fix-fast",
    "continuous-improvement",
    "debate-bugfix",
    "dep-upgrade",
    "doc-sprint",
    "hotfix",
    "multi-component-refactor",
    "onboarding",
    "perf-regression",
    "security-audit",
    "standard-cicd",
    "tournament",
];

/// Resolve the `agent/templates/` directory relative to the gateway crate's
/// `CARGO_MANIFEST_DIR` (i.e. `<repo>/gateway` → `<repo>/agent/templates`).
fn templates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("agent")
        .join("templates")
}

/// Enumerate every `*.json` file under `agent/templates/`, sorted by file
/// name for deterministic test output.
fn list_template_files() -> Vec<PathBuf> {
    let dir = templates_dir();
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", dir.display(), e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
}

/// Extract the step ID referenced by a condition string.
///
/// Conditions are of the form `<step_id>.result.<path> <op> <value>`, e.g.
/// `implement.result.exit_code == 0`. The step ID is the first `.`-separated
/// segment. Returns `None` for empty / unparseable conditions.
fn condition_step_id(condition: &str) -> Option<&str> {
    let trimmed = condition.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.split('.').next().filter(|s| !s.is_empty())
}

/// Detect a cycle in the DAG using Kahn's algorithm (topological sort by
/// in-degree).
///
/// Returns `Ok(())` if all steps are processed (no cycle), or
/// `Err(String)` describing the cycle (the set of step IDs that remain
/// unprocessed).
fn detect_cycle(step_ids: &[String], depends_on: &HashMap<String, Vec<String>>) -> Result<(), String> {
    // in_degree[step] = number of unsatisfied dependencies.
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    for id in step_ids {
        in_degree.insert(id.clone(), depends_on.get(id).map(|v| v.len()).unwrap_or(0));
    }

    // Reverse adjacency: for each step, which steps depend on it?
    // When a step completes, decrement the in-degree of its dependents.
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    for id in step_ids {
        dependents.insert(id.clone(), Vec::new());
    }
    for (id, deps) in depends_on {
        for dep in deps {
            dependents
                .get_mut(dep)
                .map(|v| v.push(id.clone()))
                .ok_or_else(|| format!("dangling dependency: {} -> {}", id, dep))?;
        }
    }

    // Seed the queue with all in-degree-0 steps.
    let mut queue: Vec<String> = in_degree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    queue.sort(); // deterministic processing order

    let mut processed = 0usize;
    while let Some(step) = queue.pop() {
        processed += 1;
        if let Some(deps) = dependents.get(&step) {
            for dep in deps {
                if let Some(d) = in_degree.get_mut(dep) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        queue.push(dep.clone());
                        queue.sort();
                    }
                }
            }
        }
    }

    if processed == step_ids.len() {
        Ok(())
    } else {
        let remaining: Vec<&str> = in_degree
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(k, _)| k.as_str())
            .collect();
        Err(format!(
            "cycle detected: {} of {} steps could not be processed (remaining: {})",
            remaining.len(),
            step_ids.len(),
            remaining.join(", ")
        ))
    }
}

/// Validate a single parsed template JSON value. Returns `Ok(template_name)`
/// on success, or `Err(message)` describing the first validation failure.
fn validate_template(name: &str, template: &Value) -> Result<String, String> {
    // 1. Top-level: name (string) + dag (object).
    let template_name = template
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("[{}] missing or non-string top-level `name`", name))?;
    if template_name.is_empty() {
        return Err(format!("[{}] top-level `name` is empty", name));
    }

    let dag = template
        .get("dag")
        .and_then(|v| v.as_object())
        .ok_or_else(|| format!("[{}] missing or non-object `dag`", name))?;

    let steps = dag
        .get("steps")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("[{}] `dag.steps` missing or not an array", name))?;

    if steps.is_empty() {
        return Err(format!("[{}] `dag.steps` is empty", name));
    }

    // 2. Per-step validation + collect IDs.
    let mut step_ids: Vec<String> = Vec::with_capacity(steps.len());
    let mut step_id_set: HashSet<String> = HashSet::new();
    let mut depends_on_map: HashMap<String, Vec<String>> = HashMap::new();

    for (i, step) in steps.iter().enumerate() {
        let step_name = format!("[{}] step[{}]", name, i);

        let id = step
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{} missing or non-string `id`", step_name))?;
        if id.is_empty() {
            return Err(format!("{} `id` is empty", step_name));
        }
        if !step_id_set.insert(id.to_string()) {
            return Err(format!("{} duplicate step id `{}`", step_name, id));
        }
        step_ids.push(id.to_string());

        // 2a. `task` must be an object with instruction + image + ttl_secs.
        let task = step
            .get("task")
            .and_then(|v| v.as_object())
            .ok_or_else(|| format!("{} `{}` missing or non-object `task`", step_name, id))?;

        let instruction = task
            .get("instruction")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{} `{}` missing or non-string `task.instruction`", step_name, id))?;
        if instruction.is_empty() {
            return Err(format!("{} `{}` `task.instruction` is empty", step_name, id));
        }

        let image = task
            .get("image")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{} `{}` missing or non-string `task.image`", step_name, id))?;
        if image.is_empty() {
            return Err(format!("{} `{}` `task.image` is empty", step_name, id));
        }

        let ttl_secs = task
            .get("ttl_secs")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("{} `{}` missing or non-integer `task.ttl_secs`", step_name, id))?;
        if ttl_secs == 0 {
            return Err(format!("{} `{}` `task.ttl_secs` must be > 0", step_name, id));
        }

        // 2b. `depends_on` must be an array of strings (may be empty).
        let depends_on: Vec<String> = match step.get("depends_on") {
            None => Vec::new(),
            Some(Value::Array(arr)) => arr
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .ok_or_else(|| format!("{} `{}` depends_on contains a non-string", step_name, id))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(format!(
                    "{} `{}` `depends_on` must be an array",
                    step_name, id
                ));
            }
        };
        depends_on_map.insert(id.to_string(), depends_on.clone());

        // 3. Every dep must reference an existing step in this DAG.
        //    (Self-dependency is also a cycle; the cycle check below catches it,
        //    but we flag it explicitly here for a clearer error.)
        for dep in &depends_on {
            if dep == id {
                return Err(format!(
                    "{} `{}` depends_on references itself (`{}`)",
                    step_name, id, dep
                ));
            }
            // We can't check existence yet — the dep may refer to a step
            // later in the array. Defer to the post-pass below.
        }

        // 4. `condition` (optional) — first segment must be an existing step.
        //    We collect the reference here and validate after the loop.
        // 5. `parallel_with` (optional) — must be an existing step.
        //    Same deferral.
    }

    // 3. Post-pass: every depends_on entry must reference an existing step.
    for (id, deps) in &depends_on_map {
        for dep in deps {
            if !step_id_set.contains(dep) {
                return Err(format!(
                    "[{}] step `{}` depends_on references unknown step `{}`",
                    name, id, dep
                ));
            }
        }
    }

    // 4. Post-pass: condition + parallel_with references must be valid.
    for step in steps {
        let id = step.get("id").and_then(|v| v.as_str()).unwrap();
        let step_name = format!("[{}] step `{}`", name, id);

        if let Some(cond_val) = step.get("condition").and_then(|v| v.as_str()) {
            if let Some(referenced) = condition_step_id(cond_val) {
                if !step_id_set.contains(referenced) {
                    return Err(format!(
                        "{} `condition` references unknown step `{}` (condition: `{}`)",
                        step_name, referenced, cond_val
                    ));
                }
            }
        }

        if let Some(pw) = step.get("parallel_with").and_then(|v| v.as_str()) {
            if !step_id_set.contains(pw) {
                return Err(format!(
                    "{} `parallel_with` references unknown step `{}`",
                    step_name, pw
                ));
            }
        }
    }

    // 5. Cycle detection (Kahn's algorithm).
    detect_cycle(&step_ids, &depends_on_map)
        .map_err(|e| format!("[{}] {}", name, e))?;

    Ok(template_name.to_string())
}

// ============================================================================
// Tests
// ============================================================================

/// Validate every template file under `agent/templates/`. Asserts that:
/// - exactly `EXPECTED_TEMPLATE_COUNT` files exist
/// - every file parses as JSON
/// - every template passes all structural checks
/// - every expected template name is present (no silent deletions)
#[test]
fn all_templates_pass_structural_validation() {
    let files = list_template_files();
    assert_eq!(
        files.len(),
        EXPECTED_TEMPLATE_COUNT,
        "expected {} template files, found {}: {:?}",
        EXPECTED_TEMPLATE_COUNT,
        files.len(),
        files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>(),
    );

    let mut seen_names: HashSet<String> = HashSet::new();

    for path in &files {
        let file_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>");
        let display = path.file_name().unwrap().to_string_lossy().to_string();

        let contents = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", display, e));

        let parsed: Value = serde_json::from_str(&contents).unwrap_or_else(|e| {
            panic!("{} is not valid JSON: {}", display, e)
        });

        let template_name = validate_template(file_name, &parsed)
            .unwrap_or_else(|e| panic!("template {} failed validation: {}", display, e));

        seen_names.insert(template_name);
    }

    // Every expected template name must be present.
    for expected in EXPECTED_TEMPLATE_NAMES {
        assert!(
            seen_names.contains(*expected),
            "expected template `{}` not found in agent/templates/ (seen: {:?})",
            expected,
            seen_names
        );
    }
    assert_eq!(
        seen_names.len(),
        EXPECTED_TEMPLATE_COUNT,
        "expected {} distinct template names, got {}: {:?}",
        EXPECTED_TEMPLATE_COUNT,
        seen_names.len(),
        seen_names
    );
}

/// Per-template spot check: assert each expected template is structurally
/// valid on its own. This produces a separate test failure per template,
/// making it easier to localise regressions than the monolithic test above.
#[test]
fn each_template_individually_valid() {
    let files = list_template_files();
    assert!(
        !files.is_empty(),
        "no template files found in agent/templates/"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut checked: Vec<String> = Vec::new();

    for path in &files {
        let file_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>");
        let display = path.file_name().unwrap().to_string_lossy().to_string();

        let contents = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{}: read error: {}", display, e));
                continue;
            }
        };

        let parsed: Value = match serde_json::from_str(&contents) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}: JSON parse error: {}", display, e));
                continue;
            }
        };

        if let Err(e) = validate_template(file_name, &parsed) {
            failures.push(format!("{}: {}", display, e));
        } else {
            checked.push(display);
        }
    }

    assert!(
        failures.is_empty(),
        "template validation failures:\n  - {}\n(checked OK: {})",
        failures.join("\n  - "),
        checked.len()
    );
}

/// Cycle detection regression: a hand-rolled cyclic DAG must be rejected,
/// and a known-good acyclic DAG must be accepted. Guards against the
/// topological-sort logic silently degenerating to "always Ok".
#[test]
fn cycle_detection_logic_is_sound() {
    // Acyclic: a → b → c, plus a → c (diamond-ish). Should pass.
    let acyclic_ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let mut acyclic_deps: HashMap<String, Vec<String>> = HashMap::new();
    acyclic_deps.insert("a".to_string(), vec![]);
    acyclic_deps.insert("b".to_string(), vec!["a".to_string()]);
    acyclic_deps.insert("c".to_string(), vec!["a".to_string(), "b".to_string()]);
    assert!(
        detect_cycle(&acyclic_ids, &acyclic_deps).is_ok(),
        "acyclic DAG must not be flagged as cyclic"
    );

    // Cyclic: a → b → a. Should fail.
    let cyclic_ids = vec!["a".to_string(), "b".to_string()];
    let mut cyclic_deps: HashMap<String, Vec<String>> = HashMap::new();
    cyclic_deps.insert("a".to_string(), vec!["b".to_string()]);
    cyclic_deps.insert("b".to_string(), vec!["a".to_string()]);
    assert!(
        detect_cycle(&cyclic_ids, &cyclic_deps).is_err(),
        "cyclic DAG (a -> b -> a) must be detected"
    );

    // Self-loop: a → a. Should fail.
    let self_loop_ids = vec!["a".to_string()];
    let mut self_loop_deps: HashMap<String, Vec<String>> = HashMap::new();
    self_loop_deps.insert("a".to_string(), vec!["a".to_string()]);
    assert!(
        detect_cycle(&self_loop_ids, &self_loop_deps).is_err(),
        "self-loop (a -> a) must be detected as a cycle"
    );
}

/// `condition_step_id` correctly extracts the first segment of a condition
/// string and returns `None` for empty / unparseable inputs.
#[test]
fn condition_step_id_extracts_correctly() {
    assert_eq!(condition_step_id("implement.result.exit_code == 0"), Some("implement"));
    assert_eq!(condition_step_id("build.result.failed == 0"), Some("build"));
    assert_eq!(condition_step_id("a.b.c.d == true"), Some("a"));
    assert_eq!(condition_step_id(""), None);
    assert_eq!(condition_step_id("   "), None);
}
