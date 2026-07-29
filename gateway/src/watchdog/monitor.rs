//! Watchdog monitoring loop — runs as a background task in serve().
//!
//! Every 60 seconds, queries all active machines, computes dedication scores,
//! detects workarounds, and issues ultimata when dedication is low.

use crate::routes::AppState;
use std::collections::HashMap;
use std::time::Duration;

/// Start the watchdog monitoring loop as a background task.
///
/// Spawns a tokio task that runs forever, monitoring all active agent sessions.
/// Should be called from `serve()` after the router is built.
pub fn spawn_watchdog(state: AppState) {
    tokio::spawn(async move {
        tracing::info!("Watchdog monitoring loop started");
        // Track consecutive low-dedication counts per machine
        let mut low_dedication_counts: HashMap<String, u32> = HashMap::new();
        // Track issued ultimatum level per machine
        let mut ultimatum_levels: HashMap<String, u32> = HashMap::new();

        loop {
            if let Err(e) = monitor_cycle(
                &state,
                &mut low_dedication_counts,
                &mut ultimatum_levels,
            )
            .await
            {
                tracing::error!(error = %e, "Watchdog monitoring cycle failed");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

/// Run one monitoring cycle.
async fn monitor_cycle(
    state: &AppState,
    low_dedication_counts: &mut HashMap<String, u32>,
    ultimatum_levels: &mut HashMap<String, u32>,
) -> anyhow::Result<()> {
    // Query all active machines with their tenant and task info
    let machines = get_active_machines(state)?;
    if machines.is_empty() {
        return Ok(());
    }

    for (machine_id, _tenant_id, task_id, task_spec) in machines {
        // Fetch recent audit entries for this machine (last 5 minutes)
        let entries = get_recent_audit_entries(state, &machine_id, 5)?;
        if entries.is_empty() {
            // No activity — skip (agent may not have started yet)
            continue;
        }

        // Extract task keywords from the task spec
        let keywords = extract_keywords(&task_spec);

        // Compute progress indicators
        let progress = crate::watchdog::dedication::ProgressIndicators::from_audit_entries(&entries);

        // Compute dedication score
        let score = crate::watchdog::dedication::compute_dedication(&entries, &keywords, &progress);

        // Detect workarounds
        let recent_output = entries
            .iter()
            .map(|e| e.cmd.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let warnings = crate::watchdog::detector::detect_workarounds(&recent_output, "");

        // Store watchdog report
        store_watchdog_report(
            state,
            &machine_id,
            task_id.as_deref(),
            score.score,
            &progress,
            &warnings,
        )?;

        // Log significant findings
        if !warnings.is_empty() {
            for w in &warnings {
                tracing::warn!(
                    machine = %machine_id,
                    pattern = %w.pattern,
                    severity = %w.severity,
                    "Watchdog: workaround detected"
                );
            }
        }

        // Ultimatum logic
        let count = low_dedication_counts.entry(machine_id.clone()).or_insert(0);
        let current_level = ultimatum_levels.entry(machine_id.clone()).or_insert(0);

        if score.score < 0.3 {
            *count += 1;
            tracing::warn!(
                machine = %machine_id,
                score = score.score,
                consecutive_low = *count,
                "Watchdog: low dedication detected"
            );

            if *count >= 3 && *current_level < 1 {
                // Issue Level 1: Warning
                issue_level(state, &machine_id, task_id.as_deref(), 1).await;
                *current_level = 1;
            } else if *count >= 5 && *current_level < 2 {
                // Issue Level 2: Directive
                issue_level(state, &machine_id, task_id.as_deref(), 2).await;
                *current_level = 2;
            } else if *count >= 7 && *current_level < 3 {
                // Issue Level 3: Escalation
                issue_level(state, &machine_id, task_id.as_deref(), 3).await;
                *current_level = 3;
            }
        } else {
            // Dedication recovered — reset counters
            *count = 0;
            if *current_level > 0 {
                tracing::info!(
                    machine = %machine_id,
                    score = score.score,
                    "Watchdog: dedication recovered, resetting ultimatum level"
                );
                *current_level = 0;
            }
        }
    }

    Ok(())
}

/// Get all active machines with their current task.
type MachineInfo = (String, String, Option<String>, String);

fn get_active_machines(
    state: &AppState,
) -> anyhow::Result<Vec<MachineInfo>> {
    let conn = state.db.get()?;
    let mut stmt = conn.prepare(
        "SELECT m.id, m.tenant_id, t.id, t.spec
         FROM machines m
         LEFT JOIN tasks t ON t.machine_id = m.id AND t.status = 'running'
         WHERE m.status = 'active'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Get recent audit entries for a machine.
fn get_recent_audit_entries(
    state: &AppState,
    machine_id: &str,
    _minutes: u32,
) -> anyhow::Result<Vec<crate::watchdog::dedication::AuditEntryRef>> {
    let conn = state.db.get()?;
    let mut stmt = conn.prepare(
        "SELECT event, payload FROM audit_entries
         WHERE machine_id = ?1
         ORDER BY seq DESC LIMIT 50",
    )?;
    let rows = stmt.query_map(rusqlite::params![machine_id], |row| {
        let event: String = row.get(0)?;
        let payload_str: String = row.get(1)?;
        let payload: serde_json::Value =
            serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
        let cmd = payload
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(crate::watchdog::dedication::AuditEntryRef { cmd, event, payload })
    })?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

/// Extract keywords from a task spec for dedication scoring.
fn extract_keywords(task_spec: &str) -> Vec<String> {
    let spec: serde_json::Value = serde_json::from_str(task_spec).unwrap_or(serde_json::Value::Null);
    let instruction = spec
        .get("instruction")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Split on whitespace and filter to words > 3 chars
    instruction
        .split_whitespace()
        .filter(|w| w.len() > 3)
        .map(|w| w.to_lowercase())
        .collect()
}

/// Store a watchdog report in the database.
fn store_watchdog_report(
    state: &AppState,
    machine_id: &str,
    task_id: Option<&str>,
    score: f64,
    progress: &crate::watchdog::dedication::ProgressIndicators,
    warnings: &[crate::watchdog::detector::WorkaroundWarning],
) -> anyhow::Result<()> {
    let conn = state.db.get()?;
    let warning_json = serde_json::to_string(warnings).unwrap_or_default();
    let assessment = if score > 0.7 {
        "Agent is on-task and making progress."
    } else if score > 0.3 {
        "Agent is partially focused."
    } else {
        "Agent appears off-task or stuck."
    };
    conn.execute(
        "INSERT INTO watchdog_reports
         (watcher_machine, watched_machine, watched_task_id, dedication_score,
          progress_files, progress_tests, progress_commits, last_activity_secs,
          workaround_warnings, ultimatum_level, assessment, created_at)
         VALUES ('watchdog-system', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, datetime('now'))",
        rusqlite::params![
            machine_id,
            task_id,
            score,
            progress.files_changed as i64,
            progress.tests_run as i64,
            progress.commits as i64,
            progress.last_activity_secs as i64,
            warning_json,
            assessment,
        ],
    )?;
    Ok(())
}

/// Issue an ultimatum at the specified level.
async fn issue_level(state: &AppState, machine_id: &str, task_id: Option<&str>, level: u32) {
    let (level_enum, message) = match level {
        1 => (
            crate::watchdog::ultimatum::UltimatumLevel::Warning,
            "You appear to be off-task. Please refocus on the assigned task.",
        ),
        2 => (
            crate::watchdog::ultimatum::UltimatumLevel::Directive,
            "You must refocus on the assigned task. Acknowledge by running: echo ACK_TASK_FOCUS",
        ),
        3 => (
            crate::watchdog::ultimatum::UltimatumLevel::Escalation,
            "Escalating to human. Agent unresponsive to ultimata.",
        ),
        _ => return,
    };

    let ultimatum = crate::watchdog::ultimatum::Ultimatum {
        level: level_enum,
        target_machine: machine_id.to_string(),
        target_task_id: task_id.map(|s| s.to_string()),
        message: message.to_string(),
        deadline_seconds: if level >= 2 { Some(120) } else { None },
    };

    if let Err(e) = crate::watchdog::ultimatum::issue_ultimatum(state, &ultimatum).await {
        tracing::error!(
            machine = %machine_id,
            level = level,
            error = %e,
            "Failed to issue ultimatum"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_keywords() {
        let spec = r#"{"instruction":"Fix the auth token expiry bug in src/auth.rs"}"#;
        let keywords = extract_keywords(spec);
        assert!(keywords.contains(&"auth".to_string()));
        assert!(keywords.contains(&"token".to_string()));
        assert!(keywords.contains(&"expiry".to_string()));
    }

    #[test]
    fn test_extract_keywords_empty_spec() {
        let keywords = extract_keywords("not json");
        assert!(keywords.is_empty());
    }
}
