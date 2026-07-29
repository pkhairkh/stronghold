//! Git workflow endpoints — clone, branch, commit, push.
//!
//! These endpoints let agents perform structured git operations against their
//! workspace pod without opening a raw PTY session. Each handler:
//!
//! 1. Verifies the `connect_token` (same SHA-256 → `machines.connect_token_hash`
//!    comparison as `exec.rs` and `pty.rs`).
//! 2. Runs the git command in the pod via `kube::Api::exec` (same pattern as
//!    `exec.rs`).
//! 3. Captures stdout / stderr / exit code.
//! 4. Writes an audit entry (event name + operation parameters — never the
//!    decrypted credential token).
//! 5. Returns a structured JSON response.
//!
//! # Endpoints
//!
//! | Method | Path                                  | Handler          | Audit event  |
//! |--------|---------------------------------------|------------------|--------------|
//! | POST   | `/agent/:machine_id/git/clone`        | [`clone_repo`]   | `git_clone`  |
//! | POST   | `/agent/:machine_id/git/branch`       | [`create_branch`]| `git_branch` |
//! | POST   | `/agent/:machine_id/git/commit`       | [`commit`]       | `git_commit` |
//! | POST   | `/agent/:machine_id/git/push`         | [`push`]         | `git_push`   |
//!
//! # Credential handling
//!
//! `clone_repo` looks up the tenant's `"github-pat"` (preferred) or
//! `"git-token"` credential from the `agent_credentials` table, decrypts it
//! with the per-tenant AES-256-GCM key (derived from the audit Ed25519
//! secret via HKDF — see [`crate::crypto::vault`]), and injects it into the
//! clone URL as `https://<token>@<repo>`. The token is **never** written to
//! the audit log or returned in the response.

use crate::crypto::vault;
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
use std::env;
use std::time::{Duration, Instant};

/// Default wall-clock timeout for git operations executed in the pod.
///
/// Git clone over a slow link can take several minutes; 10 minutes is a safe
/// upper bound that still protects against hung commands. Individual handlers
/// may pass a different timeout to [`exec_in_pod_with_timeout`].
const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 600;

/// Maximum snippet length of a commit message stored in the audit payload.
/// Keeps audit rows bounded regardless of how long the message is.
const AUDIT_MSG_SNIPPET_LEN: usize = 200;

// ============================================================================
// Clone
// ============================================================================

/// Request body for `POST /agent/:machine_id/git/clone`.
#[derive(Debug, Deserialize)]
pub struct CloneRequest {
    /// Repository to clone, e.g. `"github.com/acme/repo.git"` or
    /// `"https://github.com/acme/repo.git"`. A leading `https://` or
    /// `http://` scheme is stripped and re-added with the credential token
    /// injected (`https://<token>@<repo>`).
    pub repo: String,
    /// Branch to check out after cloning. If `None`, the repo's default
    /// branch is used.
    pub branch: Option<String>,
    /// Destination directory. If `None`, git derives a directory name from
    /// the repo URL's last path component (stripping `.git`).
    pub path: Option<String>,
}

/// Response body for `POST /agent/:machine_id/git/clone`.
#[derive(Debug, Serialize)]
pub struct CloneResponse {
    /// Process exit code. `0` on success.
    pub exit_code: i32,
    /// Combined stdout from `git clone` (and `git checkout` if requested).
    pub stdout: String,
    /// Combined stderr from `git clone` (and `git checkout` if requested).
    pub stderr: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Sequence number of the `git_clone` audit entry (`0` if the audit
    /// write failed or no row was written).
    pub audit_seq: i64,
}

/// Handle `POST /agent/:machine_id/git/clone`.
///
/// Verifies the connect token, decrypts the tenant's git credential (if any),
/// runs `git clone https://<token>@<repo> <path>` via kube exec, optionally
/// checks out the requested branch, writes a `git_clone` audit entry, and
/// returns the structured result.
pub async fn clone_repo(
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,
    State(state): State<AppState>,
    Json(req): Json<CloneRequest>,
) -> Result<Json<CloneResponse>, (StatusCode, String)> {
    // --- Step 1: verify the connect_token (same logic as exec.rs). ---
    let tenant_id = verify_connect_token(&state, &machine_id, &query).await?;

    // --- Step 2: decrypt the tenant's git credential (best-effort). ---
    // If no credential is stored, the clone proceeds without a token — public
    // repos will succeed, private repos will fail with a git error.
    let token = decrypt_git_token(&state, &tenant_id).await?;

    // --- Step 3: build the clone command. ---
    // Strip any existing scheme so we can inject the token cleanly:
    //   https://<token>@github.com/acme/repo.git [path]
    let repo_clean = strip_scheme(&req.repo);
    let url = match &token {
        Some(t) => format!("https://{}@{}", t, repo_clean),
        None => format!("https://{}", repo_clean),
    };

    let clone_dir = match &req.path {
        Some(p) => p.clone(),
        None => derive_clone_dir(&repo_clean),
    };

    let mut script = String::new();
    script.push_str("git clone ");
    script.push_str(&shell_quote(&url));
    if let Some(path) = &req.path {
        script.push(' ');
        script.push_str(&shell_quote(path));
    }
    // If a branch was requested, chain a checkout. The `&&` short-circuits
    // so checkout is skipped if the clone fails — we still capture clone's
    // stderr via the combined output.
    if let Some(branch) = &req.branch {
        script.push_str(" && cd ");
        script.push_str(&shell_quote(&clone_dir));
        script.push_str(" && git checkout ");
        script.push_str(&shell_quote(branch));
    }

    // --- Step 4: run the command. ---
    let started = Instant::now();
    let (exit_code, stdout, stderr) = exec_in_pod(&machine_id, &script).await?;
    let duration_ms = started.elapsed().as_millis() as u64;

    // --- Step 5: write the audit entry. ---
    // NOTE: payload carries the *original* repo string and branch — never the
    // decrypted token or the token-bearing URL.
    let audit_payload = serde_json::json!({
        "repo": req.repo,
        "branch": req.branch,
        "path": req.path,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
    });
    let audit_seq = write_audit_and_get_seq(
        &state,
        &tenant_id,
        &machine_id,
        "git_clone",
        audit_payload,
    )
    .await;

    tracing::info!(
        machine = %machine_id,
        repo = %req.repo,
        branch = ?req.branch,
        exit_code,
        duration_ms,
        audit_seq,
        "git clone complete"
    );

    Ok(Json(CloneResponse {
        exit_code,
        stdout,
        stderr,
        duration_ms,
        audit_seq,
    }))
}

// ============================================================================
// Branch
// ============================================================================

/// Request body for `POST /agent/:machine_id/git/branch`.
#[derive(Debug, Deserialize)]
pub struct BranchRequest {
    /// Name of the new branch.
    pub name: String,
    /// Starting point for the new branch. If `None`, the current `HEAD` is
    /// used (i.e. `git checkout -b <name>`).
    pub from: Option<String>,
}

/// Response body for `POST /agent/:machine_id/git/branch`.
#[derive(Debug, Serialize)]
pub struct BranchResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub audit_seq: i64,
}

/// Handle `POST /agent/:machine_id/git/branch`.
///
/// Runs `git checkout -b <name> [<from>]` in the pod, writes a `git_branch`
/// audit entry, and returns the structured result.
pub async fn create_branch(
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,
    State(state): State<AppState>,
    Json(req): Json<BranchRequest>,
) -> Result<Json<BranchResponse>, (StatusCode, String)> {
    let tenant_id = verify_connect_token(&state, &machine_id, &query).await?;

    // Build: git checkout -b '<name>' ['<from>']
    let mut script = String::from("git checkout -b ");
    script.push_str(&shell_quote(&req.name));
    if let Some(from) = &req.from {
        script.push(' ');
        script.push_str(&shell_quote(from));
    }

    let started = Instant::now();
    let (exit_code, stdout, stderr) = exec_in_pod(&machine_id, &script).await?;
    let duration_ms = started.elapsed().as_millis() as u64;

    let audit_payload = serde_json::json!({
        "name": req.name,
        "from": req.from,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
    });
    let audit_seq =
        write_audit_and_get_seq(&state, &tenant_id, &machine_id, "git_branch", audit_payload).await;

    tracing::info!(
        machine = %machine_id,
        branch = %req.name,
        from = ?req.from,
        exit_code,
        duration_ms,
        audit_seq,
        "git branch complete"
    );

    Ok(Json(BranchResponse {
        exit_code,
        stdout,
        stderr,
        duration_ms,
        audit_seq,
    }))
}

// ============================================================================
// Commit
// ============================================================================

/// Request body for `POST /agent/:machine_id/git/commit`.
#[derive(Debug, Deserialize)]
pub struct CommitRequest {
    /// Commit message.
    pub message: String,
    /// Specific files to stage. If `None` or empty, `git add -A` stages all
    /// changes in the working tree.
    pub files: Option<Vec<String>>,
}

/// Response body for `POST /agent/:machine_id/git/commit`.
#[derive(Debug, Serialize)]
pub struct CommitResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Parsed commit SHA (abbreviated, 7–40 hex chars) extracted from the
    /// `git commit` output. `None` if the commit failed or the SHA could
    /// not be parsed.
    pub commit_sha: Option<String>,
    pub duration_ms: u64,
    pub audit_seq: i64,
}

/// Handle `POST /agent/:machine_id/git/commit`.
///
/// Stages files (`git add <files>` or `git add -A`), runs `git commit -m
/// <message>`, parses the commit SHA from the output, writes a `git_commit`
/// audit entry, and returns the structured result.
pub async fn commit(
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,
    State(state): State<AppState>,
    Json(req): Json<CommitRequest>,
) -> Result<Json<CommitResponse>, (StatusCode, String)> {
    let tenant_id = verify_connect_token(&state, &machine_id, &query).await?;

    // Build the combined script: stage && commit.
    // `git add '<f1>' '<f2>' ...` or `git add -A`.
    let mut script = String::from("git add ");
    let files_specified = req
        .files
        .as_ref()
        .map(|f| !f.is_empty())
        .unwrap_or(false);
    if files_specified {
        for f in req.files.as_ref().unwrap() {
            script.push_str(&shell_quote(f));
            script.push(' ');
        }
        // Trailing space is harmless before " && git commit"; skip trimming.
    } else {
        script.push_str("-A");
    }

    script.push_str(" && git commit -m ");
    script.push_str(&shell_quote(&req.message));

    let started = Instant::now();
    let (exit_code, stdout, stderr) = exec_in_pod(&machine_id, &script).await?;
    let duration_ms = started.elapsed().as_millis() as u64;

    // Parse the abbreviated commit SHA from the commit output.
    // Git prints:  [main abc1234] message
    //      or:     [main (ROOT-commit) abc1234] message
    let commit_sha = if exit_code == 0 {
        parse_commit_sha(&stdout)
    } else {
        None
    };

    let msg_snippet: String = req.message.chars().take(AUDIT_MSG_SNIPPET_LEN).collect();
    let audit_payload = serde_json::json!({
        "message_snippet": msg_snippet,
        "files_count": req.files.as_ref().map(Vec::len).unwrap_or(0),
        "staged_all": !files_specified,
        "commit_sha": commit_sha,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
    });
    let audit_seq =
        write_audit_and_get_seq(&state, &tenant_id, &machine_id, "git_commit", audit_payload).await;

    tracing::info!(
        machine = %machine_id,
        commit_sha = ?commit_sha,
        exit_code,
        duration_ms,
        audit_seq,
        "git commit complete"
    );

    Ok(Json(CommitResponse {
        exit_code,
        stdout,
        stderr,
        commit_sha,
        duration_ms,
        audit_seq,
    }))
}

// ============================================================================
// Push
// ============================================================================

/// Request body for `POST /agent/:machine_id/git/push`.
#[derive(Debug, Deserialize)]
pub struct PushRequest {
    /// Remote to push to. Defaults to `"origin"`.
    pub remote: Option<String>,
    /// Branch to push. If `None`, git pushes the current branch
    /// (`git push <remote> HEAD`).
    pub branch: Option<String>,
}

/// Response body for `POST /agent/:machine_id/git/push`.
#[derive(Debug, Serialize)]
pub struct PushResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub audit_seq: i64,
}

/// Handle `POST /agent/:machine_id/git/push`.
///
/// Runs `git push <remote> <branch>` (defaults: `origin`, current `HEAD`)
/// via kube exec, writes a `git_push` audit entry, and returns the structured
/// result.
pub async fn push(
    Path(machine_id): Path<String>,
    Query(query): Query<PtyQuery>,
    State(state): State<AppState>,
    Json(req): Json<PushRequest>,
) -> Result<Json<PushResponse>, (StatusCode, String)> {
    let tenant_id = verify_connect_token(&state, &machine_id, &query).await?;

    let remote = req.remote.as_deref().unwrap_or("origin");
    let branch = req.branch.as_deref().unwrap_or("HEAD");

    let mut script = String::from("git push ");
    script.push_str(&shell_quote(remote));
    script.push(' ');
    script.push_str(&shell_quote(branch));

    let started = Instant::now();
    let (exit_code, stdout, stderr) = exec_in_pod(&machine_id, &script).await?;
    let duration_ms = started.elapsed().as_millis() as u64;

    let audit_payload = serde_json::json!({
        "remote": remote,
        "branch": branch,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
    });
    let audit_seq =
        write_audit_and_get_seq(&state, &tenant_id, &machine_id, "git_push", audit_payload).await;

    tracing::info!(
        machine = %machine_id,
        remote = %remote,
        branch = %branch,
        exit_code,
        duration_ms,
        audit_seq,
        "git push complete"
    );

    Ok(Json(PushResponse {
        exit_code,
        stdout,
        stderr,
        duration_ms,
        audit_seq,
    }))
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Verify the `connect_token` against `machines.connect_token_hash`.
///
/// Extracted from the common pattern in `exec.rs` / `pty.rs`: hash the token
/// with SHA-256, compare with the stored hash, and look up the `tenant_id`
/// for audit attribution. Returns the `tenant_id` on success.
///
/// # Errors
///
/// Returns `(401, ...)` if the token is missing/empty or doesn't match.
/// Returns `(503, ...)` if the DB pool is exhausted. Returns `(500, ...)`
/// if the machine row can't be read.
async fn verify_connect_token(
    state: &AppState,
    machine_id: &str,
    query: &PtyQuery,
) -> Result<String, (StatusCode, String)> {
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
            tracing::info!(machine = %machine_id, "git: connect_token verified");
        }
        Some(_) => {
            tracing::warn!(
                machine = %machine_id,
                "git: connect_token mismatch (HTTP 401)"
            );
            return Err((
                StatusCode::UNAUTHORIZED,
                "invalid connect token".to_string(),
            ));
        }
        None => {
            // Backward-compat: machines created before migration 002 have no
            // stored hash. Accept with a warning, matching pty.rs / exec.rs.
            tracing::warn!(
                machine = %machine_id,
                "git: no connect_token_hash stored — accepting with warning"
            );
        }
    }

    let tenant_id: String = conn
        .query_row(
            "SELECT tenant_id FROM machines WHERE id = ?1",
            rusqlite::params![machine_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "unknown".to_string());

    // Release the pooled connection before the (potentially long) kube exec.
    drop(conn);

    Ok(tenant_id)
}

/// Decrypt the tenant's git credential from the `agent_credentials` vault.
///
/// Looks up a credential named `"github-pat"` (preferred) or `"git-token"`,
/// decrypts it with the per-tenant AES-256-GCM key (derived from the audit
/// Ed25519 secret via HKDF), and returns the plaintext token.
///
/// Returns `Ok(None)` if no matching credential exists — callers may proceed
/// without a token (public repos will succeed). Returns `Err` only on DB or
/// decryption failure.
async fn decrypt_git_token(
    state: &AppState,
    tenant_id: &str,
) -> Result<Option<String>, (StatusCode, String)> {
    let conn = state.db.get().map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("db pool exhausted: {e}"),
        )
    })?;

    // Prefer "github-pat" over "git-token".
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT encrypted_value, nonce FROM agent_credentials
             WHERE tenant_id = ?1 AND name IN ('github-pat', 'git-token')
             ORDER BY CASE name WHEN 'github-pat' THEN 0 ELSE 1 END
             LIMIT 1",
            rusqlite::params![tenant_id],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .ok();

    drop(conn);

    let (ciphertext, nonce) = match row {
        Some(v) => v,
        None => {
            tracing::info!(
                tenant = %tenant_id,
                "git clone: no github-pat / git-token credential stored — cloning without auth"
            );
            return Ok(None);
        }
    };

    let tenant_key = vault::derive_tenant_key(tenant_id, &state.audit_keys);
    let plaintext = vault::decrypt(&ciphertext, &nonce, &tenant_key).map_err(|e| {
        tracing::error!(error = %e, tenant = %tenant_id, "git clone: credential decrypt failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("credential decrypt failed: {e}"),
        )
    })?;

    let token = String::from_utf8(plaintext).map_err(|e| {
        tracing::error!(error = %e, "git clone: decrypted credential is not valid UTF-8");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("decrypted credential is not valid UTF-8: {e}"),
        )
    })?;

    tracing::info!(
        tenant = %tenant_id,
        token_len = token.len(),
        "git clone: decrypted git credential"
    );
    Ok(Some(token))
}

/// Write an audit entry and fetch its sequence number.
///
/// Best-effort: a failure to write the audit entry (or read back the seq)
/// does **not** fail the response — the git command already ran. Returns `0`
/// if either step fails, matching the behaviour of `exec.rs`.
async fn write_audit_and_get_seq(
    state: &AppState,
    tenant_id: &str,
    machine_id: &str,
    event: &str,
    payload: serde_json::Value,
) -> i64 {
    if let Err(e) =
        crate::audit::log::entry(&state.db, tenant_id, machine_id, event, payload, &state.audit_keys)
    {
        tracing::error!(error = %e, event = event, "Failed to write {event} audit entry");
    }

    state
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
        .unwrap_or(0)
}

/// Run a shell command in the agent's pod via `kube exec` with the default
/// timeout ([`DEFAULT_EXEC_TIMEOUT_SECS`]).
///
/// Convenience wrapper around [`exec_in_pod_with_timeout`].
async fn exec_in_pod(
    machine_id: &str,
    command: &str,
) -> Result<(i32, String, String), (StatusCode, String)> {
    exec_in_pod_with_timeout(machine_id, command, DEFAULT_EXEC_TIMEOUT_SECS).await
}

/// Run a shell command in the agent's pod via `kube exec` and capture
/// stdout / stderr / exit code.
///
/// The `command` string is passed to `sh -c` so callers **must** shell-quote
/// any interpolated arguments via [`shell_quote`]. A wall-clock timeout is
/// enforced; on expiry the kube background task is aborted and `504 Gateway
/// Timeout` is returned.
async fn exec_in_pod_with_timeout(
    machine_id: &str,
    command: &str,
    timeout_secs: u64,
) -> Result<(i32, String, String), (StatusCode, String)> {
    use tokio::io::AsyncReadExt;

    let client = get_kube_client()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("kube client: {e}")))?;
    let pods: Api<Pod> = Api::default_namespaced(client);

    let ap = AttachParams::default()
        .stdin(false)
        .stdout(true)
        .stderr(true)
        .tty(false);

    let mut exec = pods
        .exec(machine_id, vec!["sh", "-c", command], &ap)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("kube exec failed: {e}")))?;

    // Take the status future and the stdout/stderr pipes before reading.
    let status_fut = exec.take_status();
    let stdout_reader = exec.stdout();
    let stderr_reader = exec.stderr();

    let timeout_dur = Duration::from_secs(timeout_secs.max(1));

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
                format!("git command timed out after {timeout_secs}s"),
            ))
        }
    };

    outcome
}

/// Parse the process exit code from the kubelet `Status` response.
///
/// Mirrors `exec::parse_exit_code`. On success the status is `"Success"`
/// (exit 0); on failure the reason is `NonZeroExitCode` and the message
/// typically looks like `"command terminated with exit code 1"`. We grab the
/// last whitespace-separated token and parse it as an integer; if that fails
/// we fall back to `-1` (unknown).
fn parse_exit_code(status: &Status) -> i32 {
    if status.status.as_deref() == Some("Success") {
        return 0;
    }
    if let Some(msg) = &status.message {
        if let Some(token) = msg.split_whitespace().next_back() {
            if let Ok(code) = token.parse::<i32>() {
                return code;
            }
        }
    }
    -1
}

/// Get a Kubernetes client connected to the local k3s cluster.
///
/// Mirrors `exec::get_kube_client`: try in-cluster config first, then fall
/// back to inferring from `KUBECONFIG` or the default k3s kubeconfig path.
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
/// Wraps the string in `'...'` and replaces any internal `'` with `'\''`.
/// This is the standard POSIX idiom and is safe against argument injection.
/// Mirrors `exec::shell_quote`.
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

/// Strip a leading `http://` or `https://` scheme from a repo URL.
///
/// `"https://github.com/acme/repo.git"` → `"github.com/acme/repo.git"`
/// `"github.com/acme/repo.git"`         → `"github.com/acme/repo.git"`
fn strip_scheme(repo: &str) -> String {
    if let Some(rest) = repo.strip_prefix("https://") {
        rest.to_string()
    } else if let Some(rest) = repo.strip_prefix("http://") {
        rest.to_string()
    } else {
        repo.to_string()
    }
}

/// Derive the default clone directory from a repo URL.
///
/// `"github.com/acme/repo.git"` → `"repo"`
/// `"github.com/acme/repo"`     → `"repo"`
///
/// Matches git's own behaviour: take the last path component and strip a
/// trailing `.git`.
fn derive_clone_dir(repo: &str) -> String {
    let last = repo.rsplit('/').next().unwrap_or(repo);
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// Parse the abbreviated commit SHA from `git commit` output.
///
/// Git prints the commit summary on stderr (not stdout), but we scan the
/// combined output for robustness. The SHA appears in brackets:
///   `[main abc1234] commit message`
///   `[main (ROOT-commit) abc1234] initial commit`
///
/// We extract the last 7–40 hex-char token inside the first `[...]` group.
/// Returns `None` if no SHA can be found.
fn parse_commit_sha(stdout: &str) -> Option<String> {
    // Find the first `[...]` group in the output.
    let open = stdout.find('[')?;
    let close = stdout[open..].find(']').map(|c| open + c)?;
    let inner = &stdout[open + 1..close];

    // The SHA is the last whitespace-separated token that is all hex and
    // 7–40 chars long (git's abbreviated SHA range).
    inner
        .split_whitespace()
        .rev()
        .find(|tok| {
            (7..=40).contains(&tok.len())
                && tok.chars().all(|c| c.is_ascii_hexdigit())
        })
        .map(|s| s.to_string())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ─── CloneRequest / CloneResponse ──────────────────────────────────

    #[test]
    fn test_clone_request_deserialize_full() {
        let json = r#"{"repo":"github.com/acme/repo.git","branch":"main","path":"/workspace/repo"}"#;
        let req: CloneRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.repo, "github.com/acme/repo.git");
        assert_eq!(req.branch.as_deref(), Some("main"));
        assert_eq!(req.path.as_deref(), Some("/workspace/repo"));
    }

    #[test]
    fn test_clone_request_deserialize_minimal() {
        let json = r#"{"repo":"github.com/acme/repo.git"}"#;
        let req: CloneRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.repo, "github.com/acme/repo.git");
        assert!(req.branch.is_none());
        assert!(req.path.is_none());
    }

    #[test]
    fn test_clone_request_deserialize_with_scheme() {
        let json = r#"{"repo":"https://github.com/acme/repo.git","branch":"dev"}"#;
        let req: CloneRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.repo, "https://github.com/acme/repo.git");
        assert_eq!(req.branch.as_deref(), Some("dev"));
    }

    #[test]
    fn test_clone_request_missing_repo_fails() {
        let json = r#"{"branch":"main"}"#;
        assert!(serde_json::from_str::<CloneRequest>(json).is_err());
    }

    #[test]
    fn test_clone_response_serialize_success() {
        let resp = CloneResponse {
            exit_code: 0,
            stdout: "Cloning into 'repo'...\n".to_string(),
            stderr: String::new(),
            duration_ms: 1234,
            audit_seq: 5,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 5);
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["stdout"], "Cloning into 'repo'...\n");
        assert_eq!(v["stderr"], "");
        assert_eq!(v["duration_ms"], 1234);
        assert_eq!(v["audit_seq"], 5);
    }

    #[test]
    fn test_clone_response_serialize_failure() {
        let resp = CloneResponse {
            exit_code: 128,
            stdout: String::new(),
            stderr: "fatal: repository not found\n".to_string(),
            duration_ms: 500,
            audit_seq: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"exit_code\":128"));
        assert!(json.contains("\"stderr\":\"fatal: repository not found\\n\""));
    }

    // ─── BranchRequest / BranchResponse ────────────────────────────────

    #[test]
    fn test_branch_request_deserialize_full() {
        let json = r#"{"name":"feature-xyz","from":"main"}"#;
        let req: BranchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "feature-xyz");
        assert_eq!(req.from.as_deref(), Some("main"));
    }

    #[test]
    fn test_branch_request_deserialize_no_from() {
        let json = r#"{"name":"hotfix-1"}"#;
        let req: BranchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "hotfix-1");
        assert!(req.from.is_none());
    }

    #[test]
    fn test_branch_request_missing_name_fails() {
        let json = r#"{"from":"main"}"#;
        assert!(serde_json::from_str::<BranchRequest>(json).is_err());
    }

    #[test]
    fn test_branch_response_serialize() {
        let resp = BranchResponse {
            exit_code: 0,
            stdout: "Switched to a new branch 'feature-xyz'\n".to_string(),
            stderr: String::new(),
            duration_ms: 50,
            audit_seq: 3,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(obj_len(&v), 5);
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["duration_ms"], 50);
        assert_eq!(v["audit_seq"], 3);
    }

    // ─── CommitRequest / CommitResponse ────────────────────────────────

    #[test]
    fn test_commit_request_deserialize_with_files() {
        let json = r#"{"message":"fix: handle nil pointer","files":["src/main.rs","src/lib.rs"]}"#;
        let req: CommitRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "fix: handle nil pointer");
        assert_eq!(
            req.files.as_ref().unwrap(),
            &vec!["src/main.rs".to_string(), "src/lib.rs".to_string()]
        );
    }

    #[test]
    fn test_commit_request_deserialize_no_files() {
        let json = r#"{"message":"wip"}"#;
        let req: CommitRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "wip");
        assert!(req.files.is_none());
    }

    #[test]
    fn test_commit_request_deserialize_empty_files() {
        let json = r#"{"message":"wip","files":[]}"#;
        let req: CommitRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "wip");
        assert!(req.files.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_commit_request_missing_message_fails() {
        let json = r#"{"files":["a.rs"]}"#;
        assert!(serde_json::from_str::<CommitRequest>(json).is_err());
    }

    #[test]
    fn test_commit_response_serialize_with_sha() {
        let resp = CommitResponse {
            exit_code: 0,
            stdout: String::new(),
            stderr: "[main abc1234] fix: handle nil pointer\n 2 files changed\n".to_string(),
            commit_sha: Some("abc1234".to_string()),
            duration_ms: 80,
            audit_seq: 7,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(obj_len(&v), 6);
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["commit_sha"], "abc1234");
        assert_eq!(v["duration_ms"], 80);
        assert_eq!(v["audit_seq"], 7);
    }

    #[test]
    fn test_commit_response_serialize_without_sha() {
        let resp = CommitResponse {
            exit_code: 1,
            stdout: String::new(),
            stderr: "nothing to commit\n".to_string(),
            commit_sha: None,
            duration_ms: 30,
            audit_seq: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["commit_sha"], serde_json::Value::Null);
        assert_eq!(v["exit_code"], 1);
    }

    // ─── PushRequest / PushResponse ────────────────────────────────────

    #[test]
    fn test_push_request_deserialize_full() {
        let json = r#"{"remote":"origin","branch":"main"}"#;
        let req: PushRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.remote.as_deref(), Some("origin"));
        assert_eq!(req.branch.as_deref(), Some("main"));
    }

    #[test]
    fn test_push_request_deserialize_minimal() {
        let json = r#"{}"#;
        let req: PushRequest = serde_json::from_str(json).unwrap();
        assert!(req.remote.is_none());
        assert!(req.branch.is_none());
    }

    #[test]
    fn test_push_request_deserialize_remote_only() {
        let json = r#"{"remote":"upstream"}"#;
        let req: PushRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.remote.as_deref(), Some("upstream"));
        assert!(req.branch.is_none());
    }

    #[test]
    fn test_push_response_serialize_success() {
        let resp = PushResponse {
            exit_code: 0,
            stdout: String::new(),
            stderr: "To github.com:acme/repo.git\n   abc1234..def5678  main -> main\n".to_string(),
            duration_ms: 2000,
            audit_seq: 9,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(obj_len(&v), 5);
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["duration_ms"], 2000);
        assert_eq!(v["audit_seq"], 9);
    }

    #[test]
    fn test_push_response_serialize_rejected() {
        let resp = PushResponse {
            exit_code: 1,
            stdout: String::new(),
            stderr: " ! [rejected]    main -> main (fetch first)\n".to_string(),
            duration_ms: 100,
            audit_seq: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"exit_code\":1"));
    }

    // ─── parse_commit_sha ──────────────────────────────────────────────

    #[test]
    fn test_parse_commit_sha_normal() {
        let out = "[main abc1234] fix: handle nil pointer\n 2 files changed\n";
        assert_eq!(parse_commit_sha(out), Some("abc1234".to_string()));
    }

    #[test]
    fn test_parse_commit_sha_root_commit() {
        let out = "[main (ROOT-commit) def5678] initial commit\n";
        assert_eq!(parse_commit_sha(out), Some("def5678".to_string()));
    }

    #[test]
    fn test_parse_commit_sha_long_sha() {
        let out = "[main 0123456789abcdef0123456789abcdef01234567] msg\n";
        assert_eq!(
            parse_commit_sha(out),
            Some("0123456789abcdef0123456789abcdef01234567".to_string())
        );
    }

    #[test]
    fn test_parse_commit_sha_no_brackets() {
        let out = "nothing to commit, working tree clean\n";
        assert_eq!(parse_commit_sha(out), None);
    }

    #[test]
    fn test_parse_commit_sha_empty() {
        assert_eq!(parse_commit_sha(""), None);
    }

    #[test]
    fn test_parse_commit_sha_no_hex_in_brackets() {
        let out = "[main] some message\n";
        assert_eq!(parse_commit_sha(out), None);
    }

    #[test]
    fn test_parse_commit_sha_picks_last_hex_token() {
        // "[feature-branch (ROOT-commit) cafebabe]" — should pick cafebabe,
        // not "feature-branch" (which has a hyphen, not hex).
        let out = "[feature-branch (ROOT-commit) cafebabe] msg\n";
        assert_eq!(parse_commit_sha(out), Some("cafebabe".to_string()));
    }

    // ─── strip_scheme ──────────────────────────────────────────────────

    #[test]
    fn test_strip_scheme_https() {
        assert_eq!(
            strip_scheme("https://github.com/acme/repo.git"),
            "github.com/acme/repo.git"
        );
    }

    #[test]
    fn test_strip_scheme_http() {
        assert_eq!(
            strip_scheme("http://gitlab.com/acme/repo.git"),
            "gitlab.com/acme/repo.git"
        );
    }

    #[test]
    fn test_strip_scheme_none() {
        assert_eq!(
            strip_scheme("github.com/acme/repo.git"),
            "github.com/acme/repo.git"
        );
    }

    #[test]
    fn test_strip_scheme_preserves_rest() {
        // Only the scheme is stripped — embedded colons elsewhere are kept.
        assert_eq!(strip_scheme("https://host:8080/path"), "host:8080/path");
    }

    // ─── derive_clone_dir ──────────────────────────────────────────────

    #[test]
    fn test_derive_clone_dir_with_git_suffix() {
        assert_eq!(derive_clone_dir("github.com/acme/repo.git"), "repo");
    }

    #[test]
    fn test_derive_clone_dir_without_git_suffix() {
        assert_eq!(derive_clone_dir("github.com/acme/repo"), "repo");
    }

    #[test]
    fn test_derive_clone_dir_single_component() {
        assert_eq!(derive_clone_dir("repo.git"), "repo");
    }

    #[test]
    fn test_derive_clone_dir_no_suffix_no_slash() {
        assert_eq!(derive_clone_dir("myrepo"), "myrepo");
    }

    // ─── shell_quote ───────────────────────────────────────────────────

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
        // Internal ' must be escaped as '\''.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_quote_message_with_newline() {
        // A multi-line commit message stays intact inside single quotes.
        assert_eq!(
            shell_quote("line1\nline2"),
            "'line1\nline2'"
        );
    }

    #[test]
    fn test_shell_quote_special_chars() {
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("a;rm -rf /"), "'a;rm -rf /'");
    }

    // ─── parse_exit_code ───────────────────────────────────────────────

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
    fn test_parse_exit_code_failure_no_message() {
        let status = Status {
            status: Some("Failure".to_string()),
            ..Default::default()
        };
        assert_eq!(parse_exit_code(&status), -1);
    }

    // ─── helper ────────────────────────────────────────────────────────

    /// Convenience: number of keys in a JSON object value.
    fn obj_len(v: &serde_json::Value) -> usize {
        v.as_object().map(|o| o.len()).unwrap_or(0)
    }
}
