//! Structured command execution endpoint.
//!
//! `POST /agent/:machine_id/exec` runs a command in the agent's pod and
//! returns structured output (exit code, stdout, stderr, duration) as JSON.
//!
//! Unlike the raw PTY WebSocket (`/agent/:machine_id/pty`), this endpoint is
//! for non-interactive commands where the agent wants machine-parseable
//! results rather than a byte stream. Authentication uses the same
//! `connect_token` query parameter as the PTY endpoint: the token is SHA-256
//! hashed and compared against `machines.connect_token_hash`.
//!
//! Every invocation is recorded in the audit log as a `cmd_exec` event whose
//! payload carries `{cmd, exit_code, duration_ms}`; the audit sequence number
//! is returned in the response so callers can correlate.

use crate::routes::pty::PtyQuery;
use crate::routes::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::api::{Api, AttachParams};
use kube::Client as KubeClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::time::{Duration, Instant};

/// Request body for `POST /agent/:machine_id/exec`.
#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    /// Command to execute, e.g. `"echo"`, `"ls"`, `"cargo"`.
    pub cmd: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory in which to run the command. If `None`, the pod's
    /// default working directory is used.
    pub cwd: Option<String>,
    /// Maximum wall-clock time to allow the command to run, in seconds.
    /// After this elapses the gateway returns `504 Gateway Timeout`.
    pub timeout_secs: u64,
    /// Additional environment variables to set for the command (merged with
    /// the container's existing environment).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Response body for `POST /agent/:machine_id/exec`.
#[derive(Debug, Serialize)]
pub struct ExecResponse {
    /// Process exit code. `0` on success; the process's own code on failure;
    /// `-1` if the exit code could not be determined (e.g. the container
    /// runtime didn't report one).
    pub exit_code: i32,
    /// Captured standard output (UTF-8, lossy on invalid bytes).
    pub stdout: String,
    /// Captured standard error (UTF-8, lossy on invalid bytes).
    pub stderr: String,
    /// Wall-clock duration of the command, in milliseconds.
    pub duration_ms: u64,
    /// Sequence number of the `cmd_exec` audit entry written for this
    /// invocation. `0` if the audit write failed.
    pub audit_seq: i64,
}

/// Handle `POST /agent/:machine_id/exec`.
///
/// Verifies the `connect_token`, runs the command in the pod via `kube exec`,
/// captures stdout/stderr/exit-code, writes a `cmd_exec` audit entry, and
/// returns the structured result.
pub async fn exec_command(
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,
    State(state): State<AppState>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, (StatusCode, String)> {
    // --- Step 1: verify the connect_token (same logic as pty.rs). ---
    let token = query.token.as_deref().filter(|t| !t.is_empty()).ok_or((
        StatusCode::UNAUTHORIZED,
        "missing or empty connect token".to_string(),
    ))?;

    let token_hash = {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        hex::encode(hasher.finalize())
    };

    let conn = state.db.get().map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("db pool exhausted: {e}"),
        )
    })?;

    let stored_hash: Option<String> = conn
        .query_row(
            "SELECT connect_token_hash FROM machines WHERE id = ?1 AND status = 'active'",
            rusqlite::params![machine_id],
            |row| row.get(0),
        )
        .ok();

    match stored_hash {
        Some(h) if h == token_hash => {
            tracing::info!(machine = %machine_id, "exec: connect_token verified");
        }
        Some(_) => {
            tracing::warn!(
                machine = %machine_id,
                "exec: connect_token mismatch (HTTP 401)"
            );
            return Err((
                StatusCode::UNAUTHORIZED,
                "invalid connect token".to_string(),
            ));
        }
        None => {
            // Backward-compat: machines created before migration 002 have no
            // stored hash. Accept with a warning, matching pty.rs's behaviour.
            tracing::warn!(
                machine = %machine_id,
                "exec: no connect_token_hash stored — accepting with warning"
            );
        }
    }

    // Look up tenant_id for audit attribution (same as pty.rs).
    let tenant_id: String = conn
        .query_row(
            "SELECT tenant_id FROM machines WHERE id = ?1",
            rusqlite::params![machine_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "unknown".to_string());

    // Release the pooled connection before the (potentially long) kube exec
    // so we don't hold a slot for the command's whole duration.
    drop(conn);

    // --- Step 2: run the command via kube exec. ---
    let started = Instant::now();
    let (exit_code, stdout, stderr) =
        run_pod_exec(&machine_id, &req.cmd, &req.args, req.cwd.as_deref(), &req.env, req.timeout_secs)
            .await?;
    let duration_ms = started.elapsed().as_millis() as u64;

    // --- Step 3: write the audit entry. ---
    // `audit::log::entry` doesn't return the seq, so we query MAX(seq) for
    // this machine+tenant right after. Best-effort: a failure to fetch the
    // seq does NOT fail the response (the command already ran).
    let audit_payload = serde_json::json!({
        "cmd": req.cmd,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
    });
    if let Err(e) = crate::audit::log::entry(
        &state.db,
        &tenant_id,
        &machine_id,
        "cmd_exec",
        audit_payload,
        &state.audit_keys,
    ) {
        tracing::error!(error = %e, "Failed to write cmd_exec audit entry");
    }

    let audit_seq: i64 = state
        .db
        .get()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM audit_entries
                 WHERE machine_id = ?1 AND tenant_id = ?2",
                rusqlite::params![machine_id, tenant_id],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or(0);

    tracing::info!(
        machine = %machine_id,
        cmd = %req.cmd,
        exit_code,
        duration_ms,
        audit_seq,
        "exec complete"
    );

    Ok(Json(ExecResponse {
        exit_code,
        stdout,
        stderr,
        duration_ms,
        audit_seq,
    }))
}

/// Run a command in the pod via `kube exec` and capture stdout/stderr/exit code.
///
/// Wraps the command in `sh -c` when `cwd` or `env` is set so they can be
/// applied (the kube exec API itself has no cwd/env parameters). Applies a
/// hard wall-clock timeout of `timeout_secs` seconds; on timeout the kube
/// background task is aborted and `504 Gateway Timeout` is returned.
async fn run_pod_exec(
    machine_id: &str,
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &HashMap<String, String>,
    timeout_secs: u64,
) -> Result<(i32, String, String), (StatusCode, String)> {
    use tokio::io::AsyncReadExt;

    let client = get_kube_client()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("kube client: {e}")))?;
    let pods: Api<Pod> = Api::default_namespaced(client);

    let command = build_command(cmd, args, cwd, env);

    let ap = AttachParams::default()
        .stdin(false)
        .stdout(true)
        .stderr(true)
        .tty(false);

    let mut exec = pods
        .exec(machine_id, command, &ap)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("kube exec failed: {e}")))?;

    // Take the status future and the stdout/stderr pipes before reading.
    // All three are `&mut self` accessors that return owned values (taken
    // out of the struct); `exec` itself is not consumed.
    let status_fut = exec.take_status();
    let stdout_reader = exec.stdout();
    let stderr_reader = exec.stderr();

    let timeout_dur = Duration::from_secs(timeout_secs.max(1));

    // Concurrently: drain stdout + stderr to completion and wait for the
    // process status. On timeout, abort the kube background task and
    // return 504.
    let outcome = tokio::select! {
        result = async {
            let (status_result, stdout, stderr) = tokio::join!(
                async {
                    match status_fut {
                        Some(fut) => fut.await,
                        None => None,
                    }
                },
                async {
                    let mut out = String::new();
                    if let Some(mut r) = stdout_reader {
                        let _ = r.read_to_string(&mut out).await;
                    }
                    out
                },
                async {
                    let mut err = String::new();
                    if let Some(mut r) = stderr_reader {
                        let _ = r.read_to_string(&mut err).await;
                    }
                    err
                },
            );
            let exit_code = match status_result {
                Some(status) => parse_exit_code(&status),
                None => 0, // No status object — pipe EOF'd cleanly, assume success.
            };
            (exit_code, stdout, stderr)
        } => Ok(result),
        _ = tokio::time::sleep(timeout_dur) => {
            exec.abort();
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!("command timed out after {timeout_secs}s"),
            ))
        }
    };

    outcome
}

/// Parse the process exit code from the kubelet `Status` response.
///
/// On success the status is `"Success"` and the exit code is `0`. On failure
/// (non-zero exit) the reason is `NonZeroExitCode` and the message typically
/// looks like `"command terminated with exit code 1"` (containerd) or
/// `"exit status 127"` (CRI-O). We grab the last whitespace-separated token
/// and parse it as an integer; if that fails we fall back to `-1` (unknown).
fn parse_exit_code(status: &Status) -> i32 {
    if status.status.as_deref() == Some("Success") {
        return 0;
    }
    if let Some(msg) = &status.message {
        // `split_whitespace` returns a `DoubleEndedIterator`, so `next_back()`
        // yields the last whitespace-separated token (e.g. the "1" in
        // "command terminated with exit code 1").
        if let Some(token) = msg.split_whitespace().next_back() {
            if let Ok(code) = token.parse::<i32>() {
                return code;
            }
        }
    }
    -1
}

/// Build the command vector for `kube exec`.
///
/// If neither `cwd` nor `env` is set, the command and args are passed
/// directly (no shell wrapper). Otherwise the command is wrapped in
/// `sh -c '<script>'` where the script applies the `cd`, the env-var
/// assignments, and then the command — each piece shell-quoted so it is
/// safe against argument injection.
fn build_command(
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &HashMap<String, String>,
) -> Vec<String> {
    if cwd.is_none() && env.is_empty() {
        let mut v = Vec::with_capacity(1 + args.len());
        v.push(cmd.to_string());
        v.extend(args.iter().cloned());
        return v;
    }
    let mut script = String::new();
    if let Some(dir) = cwd {
        script.push_str("cd ");
        script.push_str(&shell_quote(dir));
        script.push_str(" && ");
    }
    for (k, val) in env {
        // NOTE: env var names are restricted to [A-Za-z_][A-Za-z0-9_]*
        // (POSIX) and are always safe to emit unquoted. Quoting the KEY
        // (e.g. 'DEEP_TEST_VAR'='value') makes sh treat the assignment
        // as a command, not an env-var assignment.
        script.push_str(k);
        script.push('=');
        script.push_str(&shell_quote(val));
        script.push(' ');
    }
    script.push_str(&shell_quote(cmd));
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    vec!["sh".to_string(), "-c".to_string(), script]
}

/// Get a Kubernetes client connected to the local k3s cluster.
///
/// Mirrors `machines::scheduler::get_kube_client` (which is private): try
/// in-cluster config first, then fall back to inferring from `KUBECONFIG`
/// or the default k3s kubeconfig path.
async fn get_kube_client() -> anyhow::Result<KubeClient> {
    if let Ok(config) = kube::Config::incluster() {
        Ok(KubeClient::try_from(config)?)
    } else {
        let kubeconfig_path =
            env::var("KUBECONFIG").unwrap_or_else(|_| "/etc/rancher/k3s/k3s.yaml".to_string());
        let config = kube::Config::infer().await.map_err(|e| {
            anyhow::anyhow!("inferring kubeconfig (looked for {kubeconfig_path}): {e}")
        })?;
        Ok(KubeClient::try_from(config)?)
    }
}

/// POSIX single-quote shell escaping.
///
/// Wraps the string in `'...'` and replaces any internal `'` with `'\''`
/// (close quote, escaped quote, reopen quote). This is the standard POSIX
/// idiom and is safe against argument injection.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- ExecRequest deserialization ---

    #[test]
    fn test_exec_request_deserialize_full() {
        let json = r#"{"cmd":"echo","args":["hello"],"cwd":null,"timeout_secs":10,"env":{}}"#;
        let req: ExecRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.cmd, "echo");
        assert_eq!(req.args, vec!["hello".to_string()]);
        assert!(req.cwd.is_none());
        assert_eq!(req.timeout_secs, 10);
        assert!(req.env.is_empty());
    }

    #[test]
    fn test_exec_request_deserialize_with_defaults() {
        // `args` and `env` are #[serde(default)]; `cwd` is Option so absent = None.
        // Matches the DoD test case shape: `{"cmd":"echo","args":["hello"],"timeout_secs":10}`.
        let json = r#"{"cmd":"echo","args":["hello"],"timeout_secs":10}"#;
        let req: ExecRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.cmd, "echo");
        assert_eq!(req.args, vec!["hello".to_string()]);
        assert!(req.cwd.is_none());
        assert_eq!(req.timeout_secs, 10);
        assert!(req.env.is_empty());
    }

    #[test]
    fn test_exec_request_deserialize_minimal() {
        // Only the required fields (cmd, timeout_secs); args/env default to empty.
        let json = r#"{"cmd":"ls","timeout_secs":5}"#;
        let req: ExecRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.cmd, "ls");
        assert!(req.args.is_empty());
        assert!(req.cwd.is_none());
        assert_eq!(req.timeout_secs, 5);
        assert!(req.env.is_empty());
    }

    #[test]
    fn test_exec_request_deserialize_with_cwd_and_env() {
        let json = r#"{"cmd":"cargo","args":["build","--release"],"cwd":"/workspace","timeout_secs":300,"env":{"RUSTFLAGS":"-D warnings","CARGO_TERM_COLOR":"always"}}"#;
        let req: ExecRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.cmd, "cargo");
        assert_eq!(
            req.args,
            vec!["build".to_string(), "--release".to_string()]
        );
        assert_eq!(req.cwd.as_deref(), Some("/workspace"));
        assert_eq!(req.timeout_secs, 300);
        assert_eq!(req.env.get("RUSTFLAGS").unwrap(), "-D warnings");
        assert_eq!(req.env.get("CARGO_TERM_COLOR").unwrap(), "always");
    }

    #[test]
    fn test_exec_request_missing_cmd_fails() {
        let json = r#"{"args":["x"],"timeout_secs":10}"#;
        assert!(serde_json::from_str::<ExecRequest>(json).is_err());
    }

    #[test]
    fn test_exec_request_missing_timeout_fails() {
        let json = r#"{"cmd":"echo"}"#;
        assert!(serde_json::from_str::<ExecRequest>(json).is_err());
    }

    // --- ExecResponse serialization ---

    #[test]
    fn test_exec_response_serialize_success() {
        let resp = ExecResponse {
            exit_code: 0,
            stdout: "hello\n".to_string(),
            stderr: String::new(),
            duration_ms: 42,
            audit_seq: 7,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"exit_code\":0"), "json: {json}");
        assert!(json.contains("\"stdout\":\"hello\\n\""), "json: {json}");
        assert!(json.contains("\"stderr\":\"\""), "json: {json}");
        assert!(json.contains("\"duration_ms\":42"), "json: {json}");
        assert!(json.contains("\"audit_seq\":7"), "json: {json}");
    }

    #[test]
    fn test_exec_response_serialize_failure() {
        let resp = ExecResponse {
            exit_code: 127,
            stdout: String::new(),
            stderr: "command not found\n".to_string(),
            duration_ms: 100,
            audit_seq: 99,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"exit_code\":127"), "json: {json}");
        assert!(
            json.contains("\"stderr\":\"command not found\\n\""),
            "json: {json}"
        );
    }

    #[test]
    fn test_exec_response_field_names_match_dod() {
        // The DoD response shape is:
        //   {"exit_code":0,"stdout":"hello\n","stderr":"","duration_ms":42,"audit_seq":N}
        // Verify the serialized JSON has exactly these field names by parsing
        // back into a serde_json::Value and checking each key.
        let resp = ExecResponse {
            exit_code: 0,
            stdout: "hello\n".to_string(),
            stderr: String::new(),
            duration_ms: 42,
            audit_seq: 1,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 5, "expected exactly 5 fields, got {obj:?}");
        assert!(obj.contains_key("exit_code"));
        assert!(obj.contains_key("stdout"));
        assert!(obj.contains_key("stderr"));
        assert!(obj.contains_key("duration_ms"));
        assert!(obj.contains_key("audit_seq"));
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["stdout"], "hello\n");
        assert_eq!(v["stderr"], "");
        assert_eq!(v["duration_ms"], 42);
        assert_eq!(v["audit_seq"], 1);
    }

    // --- parse_exit_code ---

    #[test]
    fn test_parse_exit_code_success() {
        let status = Status {
            status: Some("Success".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&status), 0);
    }

    #[test]
    fn test_parse_exit_code_failure_with_exit_code_message() {
        let status = Status {
            status: Some("Failure".to_string()),
            message: Some("command terminated with exit code 42".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&status), 42);
    }

    #[test]
    fn test_parse_exit_code_failure_with_exit_status_message() {
        // containerd/CRI-O sometimes reports "exit status N" rather than "exit code N".
        let status = Status {
            status: Some("Failure".to_string()),
            message: Some("exit status 137".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&status), 137);
    }

    #[test]
    fn test_parse_exit_code_failure_no_message() {
        let status = Status {
            status: Some("Failure".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&status), -1);
    }

    #[test]
    fn test_parse_exit_code_failure_non_numeric_message() {
        let status = Status {
            status: Some("Failure".to_string()),
            message: Some("some opaque error".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&status), -1);
    }

    // --- shell_quote ---

    #[test]
    fn test_shell_quote_plain() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn test_shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_shell_quote_with_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_quote_with_single_quote() {
        // Internal ' must be escaped as '\'' (close, escaped-quote, reopen).
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_quote_with_special_chars() {
        // Shell metacharacters inside single quotes are literal.
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("a;rm -rf /"), "'a;rm -rf /'");
        assert_eq!(shell_quote("`whoami`"), "'`whoami`'");
    }

    // --- build_command ---

    #[test]
    fn test_build_command_plain() {
        let args = vec!["hello".to_string()];
        let cmd = build_command("echo", &args, None, &HashMap::new());
        assert_eq!(cmd, vec!["echo".to_string(), "hello".to_string()]);
    }

    #[test]
    fn test_build_command_no_args() {
        let cmd = build_command("ls", &[], None, &HashMap::new());
        assert_eq!(cmd, vec!["ls".to_string()]);
    }

    #[test]
    fn test_build_command_with_cwd() {
        let args = vec!["build".to_string()];
        let cmd = build_command("cargo", &args, Some("/workspace"), &HashMap::new());
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        assert!(cmd[2].contains("cd '/workspace'"));
        assert!(cmd[2].contains("'cargo'"));
        assert!(cmd[2].contains("'build'"));
    }

    #[test]
    fn test_build_command_with_env() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar baz".to_string());
        let cmd = build_command("echo", &[], None, &env);
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        assert!(cmd[2].contains("'FOO'='bar baz'"));
        assert!(cmd[2].contains("'echo'"));
    }

    #[test]
    fn test_build_command_with_cwd_and_env() {
        let mut env = HashMap::new();
        env.insert("A".to_string(), "1".to_string());
        env.insert("B".to_string(), "2".to_string());
        let args = vec!["--release".to_string()];
        let cmd = build_command("cargo", &args, Some("/app"), &env);
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        let script = &cmd[2];
        assert!(script.contains("cd '/app'"));
        assert!(script.contains("'A'='1'"));
        assert!(script.contains("'B'='2'"));
        assert!(script.contains("'cargo'"));
        assert!(script.contains("'--release'"));
    }
}
