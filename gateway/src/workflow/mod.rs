//! Workflow engine — DAG-based orchestration of structured tasks.
//!
//! A **workflow** is a directed acyclic graph (DAG) of steps. Each step is a
//! task spec (instruction + image + context) plus a set of dependencies
//! (`depends_on`) and an optional `condition` that gates whether the step
//! runs after its dependencies complete.
//!
//! The [`engine`] module implements the DAG walker (`advance_dag`) and the
//! [`executor`] module implements the per-step pod executor
//! (`execute_step`). Together:
//! - [`engine::advance_dag`] finds steps whose dependencies are satisfied
//!   (ready), partitions them by `condition`, launches the runnable ones
//!   concurrently, retries failures up to `max_retries`, evaluates
//!   downstream conditions, and marks the run `completed` or `failed`.
//! - [`executor::execute_step`] schedules a fresh `wf-*` pod per step,
//!   waits for `Ready`, runs `sh -c "<task>"` via `kube exec`, captures
//!   stdout / stderr / exit_code, kills the pod, and writes audit entries.
//!
//! # Module tree
//! - [`engine`] — the async DAG walker (`advance_dag`, `execute`).
//! - [`executor`] — the per-step pod executor (`execute_step`).
pub mod engine;
pub mod executor;
