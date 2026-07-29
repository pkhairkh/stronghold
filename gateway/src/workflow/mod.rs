//! Workflow engine — DAG-based orchestration of structured tasks.
//!
//! A **workflow** is a directed acyclic graph (DAG) of steps. Each step is a
//! task spec (instruction + image + context) plus a set of dependencies
//! (`depends_on`) and an optional `condition` that gates whether the step
//! runs after its dependencies complete.
//!
//! The [`engine`] module implements the executor that walks the DAG:
//! - finds steps whose dependencies are satisfied (ready),
//! - launches ready steps concurrently (one Task per step),
//! - polls the `tasks` table for completion,
//! - retries failed steps up to `max_retries`,
//! - evaluates conditions to decide which downstream steps to run,
//! - marks the workflow run as `completed` or `failed` and writes an audit
//!   entry.
//!
//! # Module tree
//! - [`engine`] — the async DAG executor (`execute(run_id, state)`).
pub mod engine;
