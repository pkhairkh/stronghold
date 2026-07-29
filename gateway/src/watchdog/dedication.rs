//! Dedication scoring engine for the Stronghold watchdog system.
//!
//! The dedication score estimates how on-task an agent has been over a recent
//! window of audit activity. It blends three signals:
//!
//! - **Command relevance** — fraction of recent commands that match the task
//!   keywords.
//! - **Progress rate** — proxy for tangible forward movement (files changed,
//!   tests run, commits). Capped at 1.0.
//! - **Task alignment** — 1.0 if any recent command touched a task keyword,
//!   0.5 otherwise (drift penalty), 0.0 if there is no activity at all.
//!
//! The final score is the product of the three: `score = relevance × progress
//! × alignment`. This naturally penalises agents that are inactive (0), busy
//! but off-task (alignment 0.5), or busy on-task without making progress.
//!
//! Implemented in: P2 (this file).
//! Tested by: `dedication::tests` (6 unit tests).

use serde::{Deserialize, Serialize};

/// Tangible progress signals extracted from the audit log. Used as one input
/// to the dedication score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressIndicators {
    pub files_changed: usize,
    pub tests_run: usize,
    pub commits: usize,
    pub last_activity_secs: u64,
}

/// The computed dedication score returned to the watchdog scorer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedicationScore {
    pub score: f64,
    pub relevant_commands: usize,
    pub total_commands: usize,
    pub progress_rate: f64,
    pub task_alignment: f64,
}

/// A simplified audit entry for scoring purposes.
///
/// This is a lightweight projection of the full `AuditEntry` (which carries
/// signatures, hashes, and SEV-SNP report data). The scorer only needs the
/// command string, the event name, and the raw JSON payload.
#[derive(Debug, Clone)]
pub struct AuditEntryRef {
    pub cmd: String,
    pub event: String,
    pub payload: serde_json::Value,
}

impl ProgressIndicators {
    /// Derive progress indicators from a slice of recent audit entries.
    ///
    /// Counting rules (intentionally permissive — we would rather over-count
    /// progress than miss it):
    ///
    /// - **files_changed**: command contains `git diff` or `git add`, **or**
    ///   the event name indicates a file write (`file_write`, `fs_write`,
    ///   `write_file`). Also bumped by payload hints like
    ///   `{"event": "file_write"}` for forward-compat with richer emitters.
    /// - **tests_run**: command contains `cargo test`, `npm test`, or
    ///   `pytest`.
    /// - **commits**: command contains `git commit`.
    /// - **last_activity_secs**: approximate; we assume ~1 audit entry per
    ///   5 seconds of wall-clock activity. Zero entries ⇒ zero seconds.
    pub fn from_audit_entries(entries: &[AuditEntryRef]) -> Self {
        let mut files_changed = 0usize;
        let mut tests_run = 0usize;
        let mut commits = 0usize;

        for e in entries {
            // files_changed
            if e.cmd.contains("git diff")
                || e.cmd.contains("git add")
                || e.event.contains("file_write")
                || e.event.contains("fs_write")
                || e.event.contains("write_file")
                || e.payload
                    .get("event")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("file_write") || s.contains("fs_write"))
                    .unwrap_or(false)
            {
                files_changed += 1;
            }

            // tests_run
            if e.cmd.contains("cargo test")
                || e.cmd.contains("npm test")
                || e.cmd.contains("pytest")
            {
                tests_run += 1;
            }

            // commits
            if e.cmd.contains("git commit") {
                commits += 1;
            }
        }

        // Approximate wall-clock activity: ~1 entry per 5 seconds.
        let last_activity_secs = (entries.len() as u64) * 5;

        ProgressIndicators {
            files_changed,
            tests_run,
            commits,
            last_activity_secs,
        }
    }
}

/// Compute a dedication score in `[0.0, 1.0]` for a recent window of audit
/// activity.
///
/// See the module docs for the scoring formula. Edge cases:
///
/// - Empty audit log → `score = 0.0`, `task_alignment = 0.0`,
///   `relevant_commands = 0`, `total_commands = 0`. `progress_rate` is still
///   computed from the supplied `ProgressIndicators` (callers may pass
///   pre-computed progress even when the audit window is empty).
/// - All relevant → `task_alignment = 1.0`, `relevant_commands ==
///   total_commands`, score is `progress_rate`.
/// - None relevant → `task_alignment = 0.5`, `relevant_commands = 0`, score
///   is `0.0` (the relevance factor zeroes the product).
pub fn compute_dedication(
    recent_audit_entries: &[AuditEntryRef],
    task_keywords: &[String],
    progress: &ProgressIndicators,
) -> DedicationScore {
    let total_commands = recent_audit_entries.len();
    let relevant_commands = recent_audit_entries
        .iter()
        .filter(|e| task_keywords.iter().any(|kw| e.cmd.contains(kw)))
        .count();

    let progress_rate =
        ((progress.files_changed + progress.tests_run + progress.commits) as f64 / 5.0).min(1.0);

    let task_alignment = if total_commands == 0 {
        0.0
    } else {
        let recent_output_relevant = recent_audit_entries
            .iter()
            .any(|e| task_keywords.iter().any(|kw| e.cmd.contains(kw)));
        if recent_output_relevant {
            1.0
        } else {
            0.5
        }
    };

    let score = if total_commands == 0 {
        0.0
    } else {
        (relevant_commands as f64 / total_commands as f64) * progress_rate * task_alignment
    };

    DedicationScore {
        score,
        relevant_commands,
        total_commands,
        progress_rate,
        task_alignment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kw(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn entry(cmd: &str, event: &str) -> AuditEntryRef {
        AuditEntryRef {
            cmd: cmd.to_string(),
            event: event.to_string(),
            payload: serde_json::json!({}),
        }
    }

    // 1. Empty input: no audit entries at all.
    #[test]
    fn empty_audit_returns_zero_score() {
        let progress = ProgressIndicators {
            files_changed: 0,
            tests_run: 0,
            commits: 0,
            last_activity_secs: 0,
        };
        let score = compute_dedication(&[], &kw(&["rust", "gateway"]), &progress);

        assert_eq!(score.total_commands, 0);
        assert_eq!(score.relevant_commands, 0);
        assert_eq!(score.task_alignment, 0.0);
        assert_eq!(score.score, 0.0);
        // progress_rate is derived purely from indicators, even with empty audit.
        assert_eq!(score.progress_rate, 0.0);
    }

    // 2. All-relevant: every command matches a task keyword.
    #[test]
    fn all_relevant_commands_yield_high_score() {
        let entries = vec![
            entry("cargo build -p gateway", "command_executed"),
            entry("cargo test gateway", "command_executed"),
            entry("rustc --edition 2021 main.rs", "command_executed"),
        ];
        let progress = ProgressIndicators {
            files_changed: 2,
            tests_run: 1,
            commits: 0, // sum = 3 → progress_rate = 0.6
            last_activity_secs: 15,
        };
        let keywords = kw(&["cargo", "rust", "gateway"]);
        let score = compute_dedication(&entries, &keywords, &progress);

        assert_eq!(score.total_commands, 3);
        assert_eq!(score.relevant_commands, 3);
        assert!((score.task_alignment - 1.0).abs() < f64::EPSILON);
        assert!((score.progress_rate - 0.6).abs() < f64::EPSILON);
        // score = (3/3) * 0.6 * 1.0 = 0.6
        assert!((score.score - 0.6).abs() < 1e-12);
    }

    // 3. None-relevant: no command matches any keyword.
    #[test]
    fn none_relevant_zeroes_score() {
        let entries = vec![
            entry("ls -la /tmp", "command_executed"),
            entry("cat /etc/hostname", "command_executed"),
            entry("echo hello", "command_executed"),
        ];
        let progress = ProgressIndicators {
            files_changed: 2,
            tests_run: 2,
            commits: 1, // sum = 5 → progress_rate = 1.0
            last_activity_secs: 15,
        };
        let keywords = kw(&["cargo", "rust"]);
        let score = compute_dedication(&entries, &keywords, &progress);

        assert_eq!(score.total_commands, 3);
        assert_eq!(score.relevant_commands, 0);
        assert!((score.task_alignment - 0.5).abs() < f64::EPSILON);
        assert!((score.progress_rate - 1.0).abs() < f64::EPSILON);
        // score = (0/3) * 1.0 * 0.5 = 0.0
        assert!((score.score - 0.0).abs() < 1e-12);
    }

    // 4. Partial: some commands relevant, some not. Alignment is still 1.0
    //    because `any()` triggers as soon as one relevant command is seen.
    #[test]
    fn partial_relevance_produces_intermediate_score() {
        let entries = vec![
            entry("cargo build gateway", "command_executed"), // relevant
            entry("ls -la", "command_executed"),              // irrelevant
            entry("rustc main.rs", "command_executed"),       // relevant
            entry("cat README.md", "command_executed"),       // irrelevant
        ];
        let progress = ProgressIndicators {
            files_changed: 1,
            tests_run: 0,
            commits: 0, // sum = 1 → progress_rate = 0.2
            last_activity_secs: 20,
        };
        let keywords = kw(&["cargo", "rust"]);
        let score = compute_dedication(&entries, &keywords, &progress);

        assert_eq!(score.total_commands, 4);
        assert_eq!(score.relevant_commands, 2);
        assert!((score.task_alignment - 1.0).abs() < f64::EPSILON);
        assert!((score.progress_rate - 0.2).abs() < f64::EPSILON);
        // score = (2/4) * 0.2 * 1.0 = 0.1
        assert!((score.score - 0.1).abs() < 1e-12);
    }

    // 5. High-progress: progress_rate saturates at 1.0 even with many signals.
    #[test]
    fn high_progress_caps_progress_rate_at_one() {
        let entries = vec![
            entry("cargo build gateway", "command_executed"),
            entry("cargo test gateway", "command_executed"),
        ];
        let progress = ProgressIndicators {
            files_changed: 10,
            tests_run: 5,
            commits: 3, // sum = 18 → 18/5 = 3.6, capped to 1.0
            last_activity_secs: 30,
        };
        let keywords = kw(&["cargo", "gateway"]);
        let score = compute_dedication(&entries, &keywords, &progress);

        assert_eq!(score.relevant_commands, 2);
        assert_eq!(score.total_commands, 2);
        assert!((score.task_alignment - 1.0).abs() < f64::EPSILON);
        assert!((score.progress_rate - 1.0).abs() < f64::EPSILON);
        // score = (2/2) * 1.0 * 1.0 = 1.0
        assert!((score.score - 1.0).abs() < 1e-12);
    }

    // 6. from_audit_entries: verifies the heuristics for files_changed,
    //    tests_run, commits, and the 5-sec-per-entry activity approximation.
    #[test]
    fn from_audit_entries_counts_signals_correctly() {
        let entries = vec![
            entry("git diff README.md", "command_executed"),       // files_changed
            entry("git add src/main.rs", "command_executed"),      // files_changed
            entry("git commit -m 'wip'", "command_executed"),      // commit (+ NOT files_changed)
            entry("cargo test --lib", "command_executed"),         // tests_run
            entry("pytest tests/test_api.py", "command_executed"), // tests_run
            entry("npm test", "command_executed"),                 // tests_run
            entry("ls -la", "command_executed"),                   // nothing
            AuditEntryRef {
                cmd: "internal".to_string(),
                event: "file_write".to_string(),
                payload: serde_json::json!({"path": "/x"}),
            }, // files_changed via event
        ];
        let p = ProgressIndicators::from_audit_entries(&entries);

        // git diff (1) + git add (1) + file_write event (1) = 3.
        // (git commit is intentionally NOT counted as a file change.)
        assert_eq!(p.files_changed, 3);
        assert_eq!(p.tests_run, 3);
        assert_eq!(p.commits, 1);
        // 8 entries × 5 sec = 40 sec
        assert_eq!(p.last_activity_secs, 40);
    }
}
