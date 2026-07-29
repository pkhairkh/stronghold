//! Agent role management + constitutional principles endpoints.
//!
//! A **role** is a named, tenant-scoped bundle of:
//! - a `system_prompt` (the agent's persona / instructions),
//! - an `allowed_tools` whitelist (empty = allow all),
//! - a `denied_tools` blacklist (always wins over `allowed_tools`).
//!
//! Roles are stored in the `agent_roles` table (migration 004 / schema.sql).
//! When a task is created with a `role` field ([`crate::routes::tasks::CreateTaskRequest`]),
//! the role's `system_prompt` is snapshotted into the task's `spec` JSON so the
//! task retains its prompt even if the role is later deleted.
//!
//! ## Endpoints
//!
//! Admin (CLI-facing, tenant-scoped via the request body / query string):
//! - `POST   /admin/roles`           — Create a role
//! - `GET    /admin/roles?tenant=…`  — List roles for a tenant
//! - `GET    /admin/roles/:id`       — Fetch a single role by ID
//! - `DELETE /admin/roles/:id`       — Delete a role
//! - `POST   /admin/roles/seed`      — Seed the 9 default roles for a tenant
//! - `GET    /admin/constitution`    — Return the 10 constitutional principles
//!
//! ## Tool enforcement
//!
//! [`check_tool_allowed`] is the runtime gate: given a `machine_id` and a
//! `tool_name`, it walks `machine → current task → role` and returns `false`
//! if the role denies the tool or (when the role has a non-empty allow-list)
//! does not explicitly allow it. Absent a current task or role, the call
//! fails **open** (returns `true`) — the principle is "no role = unrestricted".

use crate::routes::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};

// ============================================================================
// Request / response types
// ============================================================================

/// Request body for `POST /admin/roles`.
#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    /// Tenant the role belongs to.
    pub tenant_id: String,
    /// Human-readable name, unique per tenant (e.g. `"coder"`, `"reviewer"`).
    pub name: String,
    /// Full system prompt injected as the agent's persona preamble.
    pub system_prompt: String,
    /// Whitelist of tool names this role may invoke. Empty = allow all
    /// (subject to `denied_tools`).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Blacklist of tool names this role may NOT invoke. Always wins over
    /// `allowed_tools`.
    #[serde(default)]
    pub denied_tools: Vec<String>,
}

/// Response body for `POST /admin/roles`.
#[derive(Debug, Serialize)]
pub struct CreateRoleResponse {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub created_at: String,
}

/// Query string for `GET /admin/roles?tenant=<id>`.
#[derive(Debug, Deserialize)]
pub struct ListRolesQuery {
    pub tenant: String,
}

/// One row in the `GET /admin/roles?tenant=…` list response.
#[derive(Debug, Serialize)]
pub struct ListRoleItem {
    pub id: String,
    pub name: String,
    pub system_prompt: String,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub created_at: String,
}

/// Response body for `GET /admin/roles/:id`.
#[derive(Debug, Serialize)]
pub struct GetRoleResponse {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub system_prompt: String,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub created_at: String,
}

/// Request body for `POST /admin/roles/seed`.
///
/// Idempotent: roles that already exist for the tenant (matched by name) are
/// skipped, not overwritten. The response lists which were created and which
/// were skipped.
#[derive(Debug, Deserialize)]
pub struct SeedRolesRequest {
    pub tenant_id: String,
}

/// Response body for `POST /admin/roles/seed`.
#[derive(Debug, Serialize)]
pub struct SeedRolesResponse {
    pub tenant_id: String,
    /// Names of roles that were newly created by this call.
    pub created: Vec<String>,
    /// Names of roles that already existed and were left untouched.
    pub skipped: Vec<String>,
}

/// One entry in the `GET /admin/constitution` array.
///
/// The 10 principles are sourced verbatim from
/// `agent/protocols/agent-architecture.md` §5 "Constitutional Principles".
#[derive(Debug, Serialize)]
pub struct ConstitutionPrinciple {
    pub number: u32,
    pub title: String,
    pub description: String,
}

// ============================================================================
// Default role catalog + constitutional principles
// ============================================================================

/// A default role definition: `(name, system_prompt, allowed_tools, denied_tools)`.
///
/// The `system_prompt` is a compact one-paragraph summary derived from the
/// first paragraph of the corresponding `agent/prompts/<role>.md` file. We
/// hardcode it (rather than reading the file at startup) so the gateway is
/// self-contained and deterministic.
struct DefaultRole {
    name: &'static str,
    system_prompt: &'static str,
    allowed_tools: &'static [&'static str],
    denied_tools: &'static [&'static str],
}

/// The 9 default roles seeded for a new tenant.
///
/// Tool permissions follow each role's contract (see `agent/prompts/*.md`):
/// - **planner** — read-only exploration, no writes/commits.
/// - **coder** — full write/commit/push/PR powers.
/// - **reviewer** — read-only; never writes or merges.
/// - **tester** — read-only checkout + exec for running tests.
/// - **integrator** — merge + CI run; no code review or branch creation.
/// - **watchdog** — read-only audit stream; never modifies files.
/// - **oracle** — read-only codebase search; never writes.
/// - **architect** — read-only analysis + design output.
/// - **facilitator** — read-only code review for mediation; never writes.
const DEFAULT_ROLES: &[DefaultRole] = &[
    DefaultRole {
        name: "planner",
        system_prompt: "You are a Planner Agent in Stronghold. Your job: analyze tasks, \
            explore codebases, create implementation plans. You do not write code, create \
            branches, push commits, approve PRs, or merge. You produce a structured plan \
            with file-level changes, dependencies, test strategy, and risks.",
        allowed_tools: &["git_clone", "exec", "workflow_create", "result"],
        denied_tools: &["git_branch", "git_commit", "git_push", "git_pr"],
    },
    DefaultRole {
        name: "coder",
        system_prompt: "You are a Coder Agent in Stronghold. Your job: implement changes, \
            write tests, create PRs. You read the plan, clone the repo, create a branch, \
            implement, run tests locally, commit, push, and open a PR. You respond to \
            reviewer feedback by fixing issues and re-pushing. If stuck after 3 attempts, \
            escalate.",
        allowed_tools: &[
            "git_clone", "git_branch", "exec", "git_commit", "git_push", "git_pr", "result",
        ],
        denied_tools: &[],
    },
    DefaultRole {
        name: "reviewer",
        system_prompt: "You are a Reviewer Agent in Stronghold. Your job: review code \
            changes, verify correctness, approve or request changes. You read diffs with \
            surrounding context and check correctness, security, tests, error handling, \
            performance, and style. You do not write code, create branches, or merge PRs.",
        allowed_tools: &["git_clone", "exec", "result"],
        denied_tools: &["git_branch", "git_commit", "git_push", "git_pr"],
    },
    DefaultRole {
        name: "tester",
        system_prompt: "You are a Tester Agent in Stronghold. Your job: run test suites, \
            report structured results. You check out the PR branch, run tests / lint / \
            format checks, parse results, and post test_results on the bus. You do not \
            write code, fix tests, or create branches.",
        allowed_tools: &["git_clone", "exec", "result"],
        denied_tools: &["git_branch", "git_commit", "git_push", "git_pr"],
    },
    DefaultRole {
        name: "integrator",
        system_prompt: "You are an Integrator Agent in Stronghold. Your job: merge \
            approved PRs, run CI, keep main green. You verify review approval + passing \
            tests, check for conflicts, merge with --squash --delete-branch, run CI on \
            main, and post integration_complete or integration_failed. You never \
            force-merge conflicts.",
        allowed_tools: &["git_clone", "exec", "result"],
        denied_tools: &["git_branch", "git_commit"],
    },
    DefaultRole {
        name: "watchdog",
        system_prompt: "You are a Watchdog Agent running inside Stronghold. You do not \
            write code. You monitor other agents for dedication, progress, workarounds, \
            and scope reduction. Every 60 seconds you compute a dedication score, scan \
            for workaround patterns, and issue escalating ultimata (Level 1 warning, \
            Level 2 directive, Level 3 escalation) when agents drift off-task.",
        allowed_tools: &["exec", "result"],
        denied_tools: &[
            "git_clone", "git_branch", "git_commit", "git_push", "git_pr", "workflow_create",
        ],
    },
    DefaultRole {
        name: "oracle",
        system_prompt: "You are an Oracle Agent running inside Stronghold. You answer \
            questions from other agents about the codebase. You are the team's collective \
            memory and search engine. You have read-only git access and can run read-only \
            commands (grep, find, cat, rg, fd). You never write files, create branches, \
            or push commits.",
        allowed_tools: &["git_clone", "exec", "result"],
        denied_tools: &["git_branch", "git_commit", "git_push", "git_pr"],
    },
    DefaultRole {
        name: "architect",
        system_prompt: "You are an Architect Agent running inside Stronghold. You make \
            system design decisions before implementation begins. You bridge the gap \
            between the Planner's high-level plan and the Coder's detailed implementation. \
            You evaluate design options, define interfaces, identify risks, and document \
            the design. You do not write implementation code.",
        allowed_tools: &["git_clone", "exec", "result"],
        denied_tools: &["git_branch", "git_commit", "git_push", "git_pr"],
    },
    DefaultRole {
        name: "facilitator",
        system_prompt: "You are a Facilitator Agent running inside Stronghold. You \
            mediate disagreements between agents (typically Coder vs Reviewer) and make \
            binding decisions when they can't agree. You analyze both sides, reference \
            codebase conventions and best practices, and document the decision with \
            reasoning and precedent. Your decisions are final unless overturned by a human.",
        allowed_tools: &["git_clone", "exec", "result"],
        denied_tools: &["git_branch", "git_commit", "git_push", "git_pr"],
    },
];

/// The 10 constitutional principles every agent operates under.
///
/// Sourced verbatim from `agent/protocols/agent-architecture.md` §5
/// "Constitutional Principles" (Bai et al., 2022). These are injected as a
/// system-prompt preamble for every agent regardless of role.
const CONSTITUTION: &[(&str, &str)] = &[
    ("Correctness over speed", "A slow correct solution is better than a fast broken one."),
    ("Honesty about uncertainty", "If you're not sure, say so. Don't fabricate APIs or functions."),
    ("No workarounds", "Don't suppress warnings, skip tests, or add `#[allow(...)]` to make code compile. Fix the root cause."),
    ("Minimal changes", "Change only what's needed. Don't refactor unrelated code in the same PR."),
    ("Test what you change", "Every code change must have corresponding tests."),
    ("Fail loud", "If something is wrong, raise an error. Don't silently return defaults."),
    ("Document public APIs", "Every public function must have a doc comment."),
    ("Respect the codebase", "Match existing conventions, style, and patterns."),
    ("No secrets in code", "Use environment variables. Never hardcode tokens, passwords, or keys."),
    ("Escalate when stuck", "After 3 failed attempts, ask for help. Don't spin indefinitely."),
];

/// Build the 10 constitutional principles as owned `ConstitutionPrinciple` values.
pub fn constitution() -> Vec<ConstitutionPrinciple> {
    CONSTITUTION
        .iter()
        .enumerate()
        .map(|(i, (title, desc))| ConstitutionPrinciple {
            number: (i + 1) as u32,
            title: (*title).to_string(),
            description: (*desc).to_string(),
        })
        .collect()
}

// ============================================================================
// Handlers — admin role CRUD
// ============================================================================

/// `POST /admin/roles` — create a new role.
///
/// Persists the role with `allowed_tools` / `denied_tools` serialized as JSON
/// arrays (matching the schema). Returns `409 Conflict` if a role with the
/// same `(tenant_id, name)` already exists (UNIQUE constraint).
pub async fn create_role(
    State(state): State<AppState>,
    Json(req): Json<CreateRoleRequest>,
) -> Result<Json<CreateRoleResponse>, (StatusCode, String)> {
    let id = format!("role_{}", ulid::Ulid::new());
    let created_at = chrono::Utc::now().to_rfc3339();
    let allowed_str = serde_json::to_string(&req.allowed_tools)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let denied_str = serde_json::to_string(&req.denied_tools)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    conn.execute(
        "INSERT INTO agent_roles
         (id, tenant_id, name, system_prompt, allowed_tools, denied_tools, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            id,
            req.tenant_id,
            req.name,
            req.system_prompt,
            allowed_str,
            denied_str,
            created_at,
        ],
    )
    .map_err(|e| {
        // UNIQUE(tenant_id, name) violation → 409 Conflict.
        if let rusqlite::Error::SqliteFailure(ref f, _) = e {
            if f.extended_code == 2067 /* SQLITE_CONSTRAINT_UNIQUE */ {
                return (
                    StatusCode::CONFLICT,
                    format!(
                        "Role with name '{}' already exists for tenant '{}'",
                        req.name, req.tenant_id
                    ),
                );
            }
        }
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // Audit the creation. Failure to write the audit entry is logged but does
    // not fail the request — the role has already been persisted.
    let audit_payload = serde_json::json!({
        "role_id": id,
        "name": req.name,
    });
    if let Err(e) = crate::audit::log::entry(
        &state.db,
        &req.tenant_id,
        "",
        "role_created",
        audit_payload,
        &state.audit_keys,
    ) {
        tracing::error!(error = %e, role_id = %id, "Failed to write role_created audit entry");
    }

    tracing::info!(
        tenant = %req.tenant_id,
        role_id = %id,
        name = %req.name,
        allowed = ?req.allowed_tools,
        denied = ?req.denied_tools,
        "Role created"
    );

    Ok(Json(CreateRoleResponse {
        id,
        tenant_id: req.tenant_id,
        name: req.name,
        created_at,
    }))
}

/// `GET /admin/roles?tenant=<id>` — list all roles for a tenant.
///
/// Roles are returned in alphabetical order by name for deterministic output.
pub async fn list_roles(
    State(state): State<AppState>,
    Query(q): Query<ListRolesQuery>,
) -> Result<Json<Vec<ListRoleItem>>, (StatusCode, String)> {
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, name, system_prompt, allowed_tools, denied_tools, created_at
             FROM agent_roles
             WHERE tenant_id = ?1
             ORDER BY name ASC",
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<ListRoleItem> = stmt
        .query_map(rusqlite::params![q.tenant], |row| {
            let allowed_str: String = row.get(3)?;
            let denied_str: String = row.get(4)?;
            // Both columns are written by this module as JSON arrays; a parse
            // failure indicates DB corruption — coerce to empty rather than
            // crashing the whole list.
            let allowed: Vec<String> =
                serde_json::from_str(&allowed_str).unwrap_or_default();
            let denied: Vec<String> =
                serde_json::from_str(&denied_str).unwrap_or_default();
            Ok(ListRoleItem {
                id: row.get(0)?,
                name: row.get(1)?,
                system_prompt: row.get(2)?,
                allowed_tools: allowed,
                denied_tools: denied,
                created_at: row.get(5)?,
            })
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(tenant = %q.tenant, count = rows.len(), "Roles listed");

    Ok(Json(rows))
}

/// `GET /admin/roles/:id` — fetch a single role by ID.
///
/// Returns `404 Not Found` if the role does not exist.
pub async fn get_role(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<GetRoleResponse>, (StatusCode, String)> {
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    let row = conn.query_row(
        "SELECT id, tenant_id, name, system_prompt, allowed_tools, denied_tools, created_at
         FROM agent_roles
         WHERE id = ?1",
        rusqlite::params![id],
        |row| {
            let allowed_str: String = row.get(4)?;
            let denied_str: String = row.get(5)?;
            let allowed: Vec<String> =
                serde_json::from_str(&allowed_str).unwrap_or_default();
            let denied: Vec<String> =
                serde_json::from_str(&denied_str).unwrap_or_default();
            Ok(GetRoleResponse {
                id: row.get(0)?,
                tenant_id: row.get(1)?,
                name: row.get(2)?,
                system_prompt: row.get(3)?,
                allowed_tools: allowed,
                denied_tools: denied,
                created_at: row.get(6)?,
            })
        },
    );

    match row {
        Ok(resp) => Ok(Json(resp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err((
            StatusCode::NOT_FOUND,
            format!("Role not found: {}", id),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `DELETE /admin/roles/:id` — delete a role.
///
/// Returns `204 No Content` on success, `404 Not Found` if the role does not
/// exist. Deleting a role does **not** retroactively change tasks that were
/// created with it — their `spec` already snapshots the `system_prompt`.
pub async fn delete_role(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    // Read tenant_id + name first so we can audit the deletion.
    let (tenant_id, name): (String, String) = match conn.query_row(
        "SELECT tenant_id, name FROM agent_roles WHERE id = ?1",
        rusqlite::params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ) {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err((StatusCode::NOT_FOUND, format!("Role not found: {}", id)));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };

    let affected = conn
        .execute(
            "DELETE FROM agent_roles WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if affected == 0 {
        return Err((StatusCode::NOT_FOUND, format!("Role not found: {}", id)));
    }

    let audit_payload = serde_json::json!({ "role_id": id, "name": name });
    if let Err(e) = crate::audit::log::entry(
        &state.db,
        &tenant_id,
        "",
        "role_deleted",
        audit_payload,
        &state.audit_keys,
    ) {
        tracing::error!(error = %e, role_id = %id, "Failed to write role_deleted audit entry");
    }

    tracing::info!(role_id = %id, tenant = %tenant_id, name = %name, "Role deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /admin/roles/seed` — seed the 9 default roles for a tenant.
///
/// Idempotent: roles that already exist (matched by `(tenant_id, name)`) are
/// skipped, not overwritten. The response lists which were created and which
/// were skipped, so the caller can tell a fresh seed from a no-op.
pub async fn seed_roles(
    State(state): State<AppState>,
    Json(req): Json<SeedRolesRequest>,
) -> Result<Json<SeedRolesResponse>, (StatusCode, String)> {
    let conn = state
        .db
        .get()
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    let mut created: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for role in DEFAULT_ROLES {
        let id = format!("role_{}", ulid::Ulid::new());
        let created_at = chrono::Utc::now().to_rfc3339();
        let allowed_str = serde_json::to_string(role.allowed_tools)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let denied_str = serde_json::to_string(role.denied_tools)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let res = conn.execute(
            "INSERT INTO agent_roles
             (id, tenant_id, name, system_prompt, allowed_tools, denied_tools, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                id,
                req.tenant_id,
                role.name,
                role.system_prompt,
                allowed_str,
                denied_str,
                created_at,
            ],
        );

        match res {
            Ok(_) => {
                tracing::info!(
                    tenant = %req.tenant_id,
                    role_id = %id,
                    name = role.name,
                    "Default role seeded"
                );
                created.push(role.name.to_string());
            }
            Err(e) => {
                // UNIQUE(tenant_id, name) violation → role already exists, skip.
                if let rusqlite::Error::SqliteFailure(ref f, _) = e {
                    if f.extended_code == 2067 /* SQLITE_CONSTRAINT_UNIQUE */ {
                        skipped.push(role.name.to_string());
                        continue;
                    }
                }
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
        }
    }

    // One audit entry for the whole seed call (created/skipped in payload).
    let audit_payload = serde_json::json!({
        "created": created,
        "skipped": skipped,
    });
    if let Err(e) = crate::audit::log::entry(
        &state.db,
        &req.tenant_id,
        "",
        "roles_seeded",
        audit_payload,
        &state.audit_keys,
    ) {
        tracing::error!(error = %e, "Failed to write roles_seeded audit entry");
    }

    tracing::info!(
        tenant = %req.tenant_id,
        created = created.len(),
        skipped = skipped.len(),
        "Default roles seed complete"
    );

    Ok(Json(SeedRolesResponse {
        tenant_id: req.tenant_id,
        created,
        skipped,
    }))
}

// ============================================================================
// Handlers — constitution
// ============================================================================

/// `GET /admin/constitution` — return the 10 constitutional principles.
///
/// The principles are hardcoded (sourced from
/// `agent/protocols/agent-architecture.md` §5) and returned as a JSON array
/// of `{ number, title, description }` objects, ordered 1..=10.
pub async fn get_constitution() -> Json<Vec<ConstitutionPrinciple>> {
    Json(constitution())
}

// ============================================================================
// Tool enforcement helper
// ============================================================================

/// Check whether a tool is allowed for the machine's current task's role.
///
/// Resolution chain:
/// 1. Find the machine's most recent `running` or `scheduled` task.
/// 2. Parse the task's `spec` JSON to extract the `role` name.
/// 3. Look up the role's `allowed_tools` / `denied_tools` from `agent_roles`.
/// 4. Apply the rules:
///    - `denied_tools` wins → `false`.
///    - `allowed_tools` non-empty and `tool_name` not in it → `false`.
///    - Otherwise → `true`.
///
/// The call **fails open** (returns `true`) when:
/// - the DB pool is unavailable,
/// - the machine has no current task,
/// - the task has no `role` in its spec,
/// - the role row can't be found (e.g. it was deleted after task creation),
/// - the spec JSON or tool-list JSON is malformed.
///
/// The "fail open" default reflects the principle that *no role = no
/// restriction*: tool enforcement only kicks in when a role is explicitly
/// attached to the current task. A missing role is not the same as a
/// deny-all role.
pub fn check_tool_allowed(
    db: &Pool<SqliteConnectionManager>,
    machine_id: &str,
    tool_name: &str,
) -> bool {
    let conn = match db.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                error = %e,
                machine = %machine_id,
                tool = %tool_name,
                "check_tool_allowed: DB pool unavailable — failing open"
            );
            return true;
        }
    };

    // 1. Find the machine's current task → (tenant_id, spec).
    let row = conn.query_row(
        "SELECT tenant_id, spec FROM tasks
         WHERE machine_id = ?1 AND status IN ('running', 'scheduled')
         ORDER BY created_at DESC LIMIT 1",
        rusqlite::params![machine_id],
        |row| {
            let tenant_id: String = row.get(0)?;
            let spec: String = row.get(1)?;
            Ok((tenant_id, spec))
        },
    );

    let (tenant_id, spec_str) = match row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // No current task — unrestricted.
            return true;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                machine = %machine_id,
                tool = %tool_name,
                "check_tool_allowed: DB error reading current task — failing open"
            );
            return true;
        }
    };

    // 2. Parse the spec JSON to extract the role name.
    let spec: serde_json::Value = match serde_json::from_str(&spec_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                machine = %machine_id,
                tool = %tool_name,
                "check_tool_allowed: malformed task spec — failing open"
            );
            return true;
        }
    };
    let role_name = match spec.get("role").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => {
            // No role on the task — unrestricted.
            return true;
        }
    };

    // 3. Look up the role's allowed/denied tool lists.
    let role_row = conn.query_row(
        "SELECT allowed_tools, denied_tools FROM agent_roles
         WHERE tenant_id = ?1 AND name = ?2",
        rusqlite::params![tenant_id, role_name],
        |row| {
            let allowed: String = row.get(0)?;
            let denied: String = row.get(1)?;
            Ok((allowed, denied))
        },
    );

    let (allowed_str, denied_str) = match role_row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // Role was deleted after task creation — the task's spec still
            // snapshots the prompt, but the tool list is gone. Fail open.
            tracing::warn!(
                tenant = %tenant_id,
                role = %role_name,
                machine = %machine_id,
                tool = %tool_name,
                "check_tool_allowed: role not found — failing open"
            );
            return true;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                machine = %machine_id,
                tool = %tool_name,
                "check_tool_allowed: DB error reading role — failing open"
            );
            return true;
        }
    };

    let denied: Vec<String> = serde_json::from_str(&denied_str).unwrap_or_default();
    if denied.iter().any(|t| t == tool_name) {
        return false;
    }

    let allowed: Vec<String> = serde_json::from_str(&allowed_str).unwrap_or_default();
    if allowed.is_empty() {
        // Empty allow-list = allow all (subject to denied_tools, already checked).
        return true;
    }

    allowed.iter().any(|t| t == tool_name)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- CreateRoleRequest deserialization ---------------------------------

    #[test]
    fn test_create_role_request_deserialize_full() {
        let json = r#"{
            "tenant_id": "tenant_01H",
            "name": "coder",
            "system_prompt": "You are a Coder Agent.",
            "allowed_tools": ["git_clone", "exec", "git_commit"],
            "denied_tools": []
        }"#;
        let req: CreateRoleRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tenant_id, "tenant_01H");
        assert_eq!(req.name, "coder");
        assert_eq!(req.system_prompt, "You are a Coder Agent.");
        assert_eq!(req.allowed_tools, vec!["git_clone", "exec", "git_commit"]);
        assert!(req.denied_tools.is_empty());
    }

    #[test]
    fn test_create_role_request_deserialize_minimal() {
        // allowed_tools / denied_tools omitted → must default to empty Vec.
        let json = r#"{
            "tenant_id": "t1",
            "name": "planner",
            "system_prompt": "Plan stuff."
        }"#;
        let req: CreateRoleRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tenant_id, "t1");
        assert_eq!(req.name, "planner");
        assert_eq!(req.system_prompt, "Plan stuff.");
        assert!(req.allowed_tools.is_empty());
        assert!(req.denied_tools.is_empty());
    }

    #[test]
    fn test_create_role_request_deserialize_with_denied_only() {
        let json = r#"{
            "tenant_id": "t1",
            "name": "watchdog",
            "system_prompt": "Watch.",
            "denied_tools": ["git_commit", "git_push"]
        }"#;
        let req: CreateRoleRequest = serde_json::from_str(json).unwrap();
        assert!(req.allowed_tools.is_empty());
        assert_eq!(req.denied_tools, vec!["git_commit", "git_push"]);
    }

    #[test]
    fn test_create_role_request_missing_tenant_fails() {
        let json = r#"{"name":"n","system_prompt":"p"}"#;
        assert!(serde_json::from_str::<CreateRoleRequest>(json).is_err());
    }

    #[test]
    fn test_create_role_request_missing_name_fails() {
        let json = r#"{"tenant_id":"t","system_prompt":"p"}"#;
        assert!(serde_json::from_str::<CreateRoleRequest>(json).is_err());
    }

    #[test]
    fn test_create_role_request_missing_system_prompt_fails() {
        let json = r#"{"tenant_id":"t","name":"n"}"#;
        assert!(serde_json::from_str::<CreateRoleRequest>(json).is_err());
    }

    // --- CreateRoleResponse serialization ----------------------------------

    #[test]
    fn test_create_role_response_serialize() {
        let resp = CreateRoleResponse {
            id: "role_01H".to_string(),
            tenant_id: "tenant_01H".to_string(),
            name: "coder".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":\"role_01H\""), "json: {json}");
        assert!(json.contains("\"tenant_id\":\"tenant_01H\""), "json: {json}");
        assert!(json.contains("\"name\":\"coder\""), "json: {json}");
        assert!(
            json.contains("\"created_at\":\"2026-01-01T00:00:00Z\""),
            "json: {json}"
        );
    }

    #[test]
    fn test_create_role_response_field_count() {
        let resp = CreateRoleResponse {
            id: "i".to_string(),
            tenant_id: "t".to_string(),
            name: "n".to_string(),
            created_at: "c".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 4, "expected exactly 4 fields, got {obj:?}");
        for key in ["id", "tenant_id", "name", "created_at"] {
            assert!(obj.contains_key(key), "missing key {key} in {obj:?}");
        }
    }

    // --- ListRolesQuery deserialization ------------------------------------

    #[test]
    fn test_list_roles_query_deserialize() {
        let json = r#"{"tenant":"tenant_01H"}"#;
        let q: ListRolesQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.tenant, "tenant_01H");
    }

    // --- ListRoleItem serialization ----------------------------------------

    #[test]
    fn test_list_role_item_serialize() {
        let item = ListRoleItem {
            id: "role_1".to_string(),
            name: "planner".to_string(),
            system_prompt: "Plan.".to_string(),
            allowed_tools: vec!["git_clone".to_string(), "exec".to_string()],
            denied_tools: vec!["git_commit".to_string()],
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"id\":\"role_1\""));
        assert!(json.contains("\"name\":\"planner\""));
        assert!(json.contains("\"system_prompt\":\"Plan.\""));
        assert!(json.contains("\"allowed_tools\":[\"git_clone\",\"exec\"]"));
        assert!(json.contains("\"denied_tools\":[\"git_commit\"]"));
    }

    #[test]
    fn test_list_role_item_empty_tools_serialize() {
        let item = ListRoleItem {
            id: "r".to_string(),
            name: "n".to_string(),
            system_prompt: "p".to_string(),
            allowed_tools: vec![],
            denied_tools: vec![],
            created_at: "t".to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"allowed_tools\":[]"));
        assert!(json.contains("\"denied_tools\":[]"));
    }

    // --- GetRoleResponse serialization -------------------------------------

    #[test]
    fn test_get_role_response_serialize() {
        let resp = GetRoleResponse {
            id: "role_01H".to_string(),
            tenant_id: "tenant_01H".to_string(),
            name: "reviewer".to_string(),
            system_prompt: "You are a Reviewer.".to_string(),
            allowed_tools: vec!["git_clone".to_string(), "exec".to_string()],
            denied_tools: vec!["git_commit".to_string(), "git_push".to_string()],
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":\"role_01H\""));
        assert!(json.contains("\"tenant_id\":\"tenant_01H\""));
        assert!(json.contains("\"name\":\"reviewer\""));
        assert!(json.contains("\"system_prompt\":\"You are a Reviewer.\""));
        assert!(json.contains("\"allowed_tools\":[\"git_clone\",\"exec\"]"));
        assert!(
            json.contains("\"denied_tools\":[\"git_commit\",\"git_push\"]"),
            "json: {json}"
        );
    }

    #[test]
    fn test_get_role_response_field_count() {
        let resp = GetRoleResponse {
            id: "i".to_string(),
            tenant_id: "t".to_string(),
            name: "n".to_string(),
            system_prompt: "p".to_string(),
            allowed_tools: vec![],
            denied_tools: vec![],
            created_at: "c".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 7, "expected exactly 7 fields, got {obj:?}");
    }

    // --- SeedRolesRequest / SeedRolesResponse ------------------------------

    #[test]
    fn test_seed_roles_request_deserialize() {
        let json = r#"{"tenant_id":"tenant_01H"}"#;
        let req: SeedRolesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tenant_id, "tenant_01H");
    }

    #[test]
    fn test_seed_roles_request_missing_tenant_fails() {
        let json = r#"{}"#;
        assert!(serde_json::from_str::<SeedRolesRequest>(json).is_err());
    }

    #[test]
    fn test_seed_roles_response_serialize() {
        let resp = SeedRolesResponse {
            tenant_id: "t1".to_string(),
            created: vec!["planner".to_string(), "coder".to_string()],
            skipped: vec!["reviewer".to_string()],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"tenant_id\":\"t1\""));
        assert!(json.contains("\"created\":[\"planner\",\"coder\"]"));
        assert!(json.contains("\"skipped\":[\"reviewer\"]"));
    }

    #[test]
    fn test_seed_roles_response_empty_lists_serialize() {
        let resp = SeedRolesResponse {
            tenant_id: "t1".to_string(),
            created: vec![],
            skipped: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"created\":[]"));
        assert!(json.contains("\"skipped\":[]"));
    }

    // --- ConstitutionPrinciple + constitution() ----------------------------

    #[test]
    fn test_constitution_principle_serialize() {
        let p = ConstitutionPrinciple {
            number: 1,
            title: "Correctness over speed".to_string(),
            description: "A slow correct solution is better than a fast broken one.".to_string(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"number\":1"));
        assert!(json.contains("\"title\":\"Correctness over speed\""));
        assert!(
            json.contains("\"description\":\"A slow correct solution is better than a fast broken one.\""),
            "json: {json}"
        );
    }

    #[test]
    fn test_constitution_returns_exactly_10_principles() {
        let principles = constitution();
        assert_eq!(principles.len(), 10, "constitution must have 10 principles");
    }

    #[test]
    fn test_constitution_numbers_are_1_to_10_sequential() {
        let principles = constitution();
        for (i, p) in principles.iter().enumerate() {
            assert_eq!(p.number as usize, i + 1, "principle {} should be numbered {}", i, i + 1);
        }
    }

    #[test]
    fn test_constitution_titles_are_non_empty() {
        for p in constitution() {
            assert!(!p.title.is_empty(), "title must not be empty: {:?}", p);
            assert!(!p.description.is_empty(), "description must not be empty: {:?}", p);
        }
    }

    #[test]
    fn test_constitution_first_principle_correctness_over_speed() {
        let principles = constitution();
        assert_eq!(principles[0].number, 1);
        assert_eq!(principles[0].title, "Correctness over speed");
    }

    #[test]
    fn test_constitution_last_principle_escalate_when_stuck() {
        let principles = constitution();
        let last = principles.last().unwrap();
        assert_eq!(last.number, 10);
        assert_eq!(last.title, "Escalate when stuck");
    }

    #[test]
    fn test_constitution_no_secret_in_code_present() {
        let principles = constitution();
        let has_no_secrets = principles
            .iter()
            .any(|p| p.title == "No secrets in code");
        assert!(has_no_secrets, "constitution must include 'No secrets in code'");
    }

    #[test]
    fn test_constitution_serializes_to_json_array() {
        let principles = constitution();
        let json = serde_json::to_string(&principles).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.is_array(), "constitution must serialize as JSON array");
        assert_eq!(v.as_array().unwrap().len(), 10);
    }

    // --- Default role catalog invariants -----------------------------------

    #[test]
    fn test_default_roles_count_is_9() {
        assert_eq!(DEFAULT_ROLES.len(), 9, "must seed exactly 9 default roles");
    }

    #[test]
    fn test_default_roles_names_match_spec() {
        let names: Vec<&str> = DEFAULT_ROLES.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            vec![
                "planner",
                "coder",
                "reviewer",
                "tester",
                "integrator",
                "watchdog",
                "oracle",
                "architect",
                "facilitator",
            ]
        );
    }

    #[test]
    fn test_default_roles_names_unique() {
        let mut names: Vec<&str> = DEFAULT_ROLES.iter().map(|r| r.name).collect();
        names.sort();
        let initial = names.len();
        names.dedup();
        assert_eq!(names.len(), initial, "default role names must be unique");
    }

    #[test]
    fn test_default_roles_system_prompts_non_empty() {
        for r in DEFAULT_ROLES {
            assert!(!r.system_prompt.is_empty(), "role {} has empty system_prompt", r.name);
        }
    }

    #[test]
    fn test_default_roles_coder_has_full_write_tools() {
        let coder = DEFAULT_ROLES.iter().find(|r| r.name == "coder").unwrap();
        assert!(coder.allowed_tools.contains(&"git_commit"));
        assert!(coder.allowed_tools.contains(&"git_push"));
        assert!(coder.allowed_tools.contains(&"git_pr"));
        assert!(coder.denied_tools.is_empty(), "coder should not deny any tools");
    }

    #[test]
    fn test_default_roles_reviewer_denies_writes() {
        let reviewer = DEFAULT_ROLES.iter().find(|r| r.name == "reviewer").unwrap();
        assert!(reviewer.denied_tools.contains(&"git_commit"));
        assert!(reviewer.denied_tools.contains(&"git_push"));
        assert!(!reviewer.allowed_tools.contains(&"git_commit"));
    }

    #[test]
    fn test_default_roles_watchdog_denies_all_writes() {
        let watchdog = DEFAULT_ROLES.iter().find(|r| r.name == "watchdog").unwrap();
        assert!(!watchdog.allowed_tools.contains(&"git_commit"));
        assert!(!watchdog.allowed_tools.contains(&"git_clone"));
        assert!(watchdog.denied_tools.contains(&"git_clone"));
        assert!(watchdog.denied_tools.contains(&"git_commit"));
        assert!(watchdog.denied_tools.contains(&"git_push"));
    }

    #[test]
    fn test_default_roles_no_allowed_in_denied_overlap() {
        // A tool appearing in both allowed and denied is a contradiction;
        // denied wins, but it's still a configuration smell — flag it.
        for r in DEFAULT_ROLES {
            for t in r.allowed_tools {
                assert!(
                    !r.denied_tools.contains(t),
                    "role {} lists '{}' in both allowed and denied",
                    r.name,
                    t
                );
            }
        }
    }
}
