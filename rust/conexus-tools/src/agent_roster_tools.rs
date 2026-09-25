//! Port of `conexus/tools/agent_roster_tools.py` (Phase D5, PR 3) —
//! the single `view_agents` peer-roster tool. Any authenticated agent
//! (worker/manager) or operator tier can list every live agent's
//! public identity + self-authored profile, answering "who do I
//! ask?" (feeds `request_assistance`). Deliberately narrow projection
//! — `{agent_id, agent_role, profile, profile_updated_at}`, no token,
//! no working directory, no secrets.
//!
//! `Requirement::Predicate` rather than `Cap`: the admission rule is
//! an OR of two capabilities from DIFFERENT bundle families
//! (`agents.use`, granted only to agent-bearers; `agents.view`,
//! granted only to operator tiers) — no single `Cap` gate spans both,
//! and collapsing to either alone would narrow who's admitted, per
//! Python's own Finding-A rationale (see `_principal_can_view_roster`'s
//! docstring).
//!
//! `list_active` (Phase B) already excludes terminated/tombstone rows;
//! this tool additionally excludes `"system"` status (the synthetic
//! system pseudo-agent, not a real peer) — matching Python's
//! `_ROSTER_EXCLUDED_STATUSES`'s extra third entry over and above
//! `list_active`'s own two-status filter.
//!
//! Deliberately NOT ported, with an explicit reason (never a silent
//! drop): `utils/audit_utils.log_audit`'s in-memory `g.audit_log` /
//! file trail — same precedent as every prior Phase D5 tool (no Rust
//! reader for it yet, and no durable `agent_actions` row exists here
//! either since Python's own call writes only to the transient trail).

use conexus_auth::{Requirement, Tool};
use conexus_core::capability::{AgentRole, Capability};
use conexus_core::principal::Principal;
use conexus_core::tool_result::ToolResult;
use conexus_db::agent_repository::AgentRepository;
use conexus_db::project_settings_repository;
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

const ROSTER_EXCLUDED_STATUSES: &[&str] = &["system"];

const ROSTER_DENIED: &str =
    "Unauthorized: An authenticated agent or operator is required to view the agent roster.";

fn principal_can_view_roster(principal: Option<&Principal>) -> bool {
    principal.is_some_and(|p| {
        p.has_capability(Capability::AgentsUse) || p.has_capability(Capability::AgentsView)
    })
}

pub struct ViewAgentsTool;

impl Tool for ViewAgentsTool {
    const NAME: &'static str = "view_agents";
    const REQUIRED: Requirement = Requirement::Predicate {
        check: principal_can_view_roster,
        reason: ROSTER_DENIED,
    };
    const DESCRIPTION: &'static str = "List every active agent on the team with their role and \
        self-authored profile (what they do, what they work on, what to ask them about). Use \
        this to find who to talk to or hand work to. Returns {\"agents\": [{agent_id, \
        agent_role, profile, profile_updated_at}, ...]}.";
    const SCHEMA: &'static str =
        r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#;

    fn call<'a>(
        _principal: Option<&'a Principal>,
        _arguments: &'a Value,
        conn: &'a AsyncMutex<Connection>,
        _now: &'a str,
        _ctx: &'a conexus_auth::ToolCallContext<'a>,
    ) -> conexus_auth::BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let conn = conn.lock().await;
            let rows = match AgentRepository::list_active(&conn) {
                Ok(rows) => rows,
                Err(_e) => {
                    return ToolResult::Failed {
                        message: "Database error reading the agent roster".to_string(),
                    }
                }
            };

            let mut roster: Vec<Value> = rows
                .into_iter()
                .filter(|r| !ROSTER_EXCLUDED_STATUSES.contains(&r.status.as_str()))
                .map(|r| {
                    serde_json::json!({
                        "agent_id": r.agent_id,
                        "agent_role": if r.agent_role.is_empty() { "worker".to_string() } else { r.agent_role },
                        "profile": r.profile,
                        "profile_updated_at": r.profile_updated_at,
                    })
                })
                .collect();
            roster.sort_by(|a, b| {
                a["agent_id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["agent_id"].as_str().unwrap_or(""))
            });

            ToolResult::Ok {
                data: Some(serde_json::json!({ "agents": roster })),
                message: None,
            }
        })
    }
}

// --- update_agent_profile -------------------------------------------
//
// ADR-0019's PR1 "one self-service tool" -- documented in `get_system_
// prompt`, backed end-to-end by `AgentRepository::review_profile` and
// the three `config_allow_*_profile` settings, but never actually
// wired up as an MCP `Tool` in the Rust port until now (found live:
// `settings_schema.rs`'s three toggles and this doc string were the
// only callers/references anywhere in the tree -- the tool itself
// didn't exist, so every agent that read its own `get_system_prompt`
// was told about a capability the server could never grant).
const PROFILE_DENIED: &str =
    "Unauthorized: An authenticated agent is required to update a profile.";

fn principal_can_use_profile_tool(principal: Option<&Principal>) -> bool {
    principal.is_some_and(|p| p.has_capability(Capability::AgentsUse))
}

pub struct UpdateAgentProfileTool;

impl Tool for UpdateAgentProfileTool {
    const NAME: &'static str = "update_agent_profile";
    const REQUIRED: Requirement = Requirement::Predicate {
        check: principal_can_use_profile_tool,
        reason: PROFILE_DENIED,
    };
    const DESCRIPTION: &'static str = "Add or confirm your own self-authored profile (what you \
        do, what you work on, what to ask you about) -- or, if you are a manager, curate a \
        worker's profile. Omit `profile` to just confirm your existing one is still accurate \
        (bumps the review timestamp without changing content). Managers may target a different \
        worker via `agent_id`; managers may not edit another manager's profile.";
    const SCHEMA: &'static str = r#"{
        "type": "object",
        "properties": {
            "profile": {
                "type": "string",
                "description": "New profile text. Omit to confirm the existing profile is still accurate."
            },
            "agent_id": {
                "type": "string",
                "description": "Target agent (manager-only curation). Omit to edit/confirm your own profile."
            }
        },
        "required": [],
        "additionalProperties": false
    }"#;

    fn call<'a>(
        principal: Option<&'a Principal>,
        arguments: &'a Value,
        conn: &'a AsyncMutex<Connection>,
        now: &'a str,
        ctx: &'a conexus_auth::ToolCallContext<'a>,
    ) -> conexus_auth::BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let principal = match principal {
                Some(p) => p,
                None => {
                    return ToolResult::PermissionDenied {
                        reason: PROFILE_DENIED.to_string(),
                    }
                }
            };
            let Some(caller_id) = principal.agent_id.as_deref() else {
                return ToolResult::PermissionDenied {
                    reason: PROFILE_DENIED.to_string(),
                };
            };
            let new_profile = arguments.get("profile").and_then(Value::as_str);
            let target_agent_id = arguments
                .get("agent_id")
                .and_then(Value::as_str)
                .unwrap_or(caller_id);
            let is_self = target_agent_id == caller_id;
            let caller_is_manager = principal.agent_role == Some(AgentRole::Manager);

            if !is_self {
                if !caller_is_manager {
                    return ToolResult::PermissionDenied {
                        reason: "Only a manager may edit another agent's profile.".to_string(),
                    };
                }
                let target_role = {
                    let conn = conn.lock().await;
                    match AgentRepository::get_by_id(&conn, target_agent_id) {
                        Ok(Some(row)) => row.agent_role,
                        Ok(None) => {
                            return ToolResult::NotFound {
                                resource: "agent".to_string(),
                                identifier: target_agent_id.to_string(),
                                hint: None,
                            }
                        }
                        Err(_) => {
                            return ToolResult::Failed {
                                message: "Database error reading the target agent".to_string(),
                            }
                        }
                    }
                };
                if target_role == "manager" {
                    return ToolResult::PermissionDenied {
                        reason: "Managers may not edit another manager's profile.".to_string(),
                    };
                }
                let allow_curate = project_settings_repository::get_bool(
                    ctx.sea_orm_db,
                    "config_allow_manager_curate_profiles",
                    true,
                )
                .await;
                if !allow_curate && new_profile.is_some() {
                    return ToolResult::PermissionDenied {
                        reason: "config_allow_manager_curate_profiles is disabled; ask an \
                            operator to enable it before curating a worker's profile."
                            .to_string(),
                    };
                }
            } else if new_profile.is_some() {
                // Content-changing self-edit -- gated by the caller's
                // own role toggle. A no-arg "confirm" call always
                // succeeds regardless (ADR-0019 decision 4).
                let toggle_key = if caller_is_manager {
                    "config_allow_manager_update_own_profile"
                } else {
                    "config_allow_worker_update_own_profile"
                };
                let allow_self_edit =
                    project_settings_repository::get_bool(ctx.sea_orm_db, toggle_key, true).await;
                if !allow_self_edit {
                    return ToolResult::PermissionDenied {
                        reason: format!(
                            "{toggle_key} is disabled; you may still confirm your profile \
                                (call with no `profile` argument) but not change its content."
                        ),
                    };
                }
            }

            // `review_profile` is sea-orm-backed (`ctx.sea_orm_db`) --
            // no rusqlite `conn` lock needed here (that mutex only
            // guarded the target-role lookup above).
            let result = AgentRepository::review_profile(
                ctx.sea_orm_db,
                target_agent_id,
                new_profile,
                Some(caller_id),
                now,
            )
            .await;

            match result {
                Ok(Some(r)) => ToolResult::Ok {
                    data: Some(serde_json::json!({
                        "agent_id": target_agent_id,
                        "profile": r.agent.profile,
                        "profile_updated_at": r.agent.profile_updated_at,
                        "profile_reviewed_at": r.agent.profile_reviewed_at,
                        "changed": r.changed,
                    })),
                    message: Some(if r.changed {
                        "Profile updated.".to_string()
                    } else {
                        "Profile confirmed (no content change).".to_string()
                    }),
                },
                Ok(None) => ToolResult::NotFound {
                    resource: "agent".to_string(),
                    identifier: target_agent_id.to_string(),
                    hint: None,
                },
                Err(_) => ToolResult::Failed {
                    message: "Database error updating the profile".to_string(),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    // Phase G (sea-orm migration infra): a throwaway in-memory
    // sea-orm connection for ToolCallContext::sea_orm_db -- no test in
    // this file queries through it yet, it only needs to exist so
    // off_wire's now-mandatory last argument has something to point
    // at.
    async fn test_sea_orm_db() -> sea_orm::DatabaseConnection {
        sea_orm::Database::connect("sqlite::memory:").await.unwrap()
    }

    use super::*;
    use conexus_auth::ToolCallContext;
    use conexus_core::capability::Capabilities;
    use conexus_core::principal::PrincipalKind;
    use conexus_db::schema::init_schema;
    use conexus_wakeloop::waiter_registry::WaiterRegistry;
    use std::collections::HashSet;

    fn agent_bearer_with(cap: Capability) -> Principal {
        Principal {
            kind: PrincipalKind::AgentBearer,
            user_id: None,
            agent_id: Some("caller-1".to_string()),
            project_name: None,
            project_role: None,
            agent_role: Some(conexus_core::capability::AgentRole::Worker),
            can_wake_loop: true,
            source_token: None,
            capabilities: Capabilities::Set(HashSet::from([cap])),
        }
    }

    async fn setup() -> AsyncMutex<Connection> {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        AsyncMutex::new(conn)
    }

    fn seed(conn: &Connection, agent_id: &str, role: &str) {
        conn.execute(
            "INSERT INTO agents (token, agent_id, created_at, status, working_directory, agent_role) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                format!("{agent_id}-tok"),
                agent_id,
                "2026-06-01T00:00:00Z",
                "active",
                "/tmp",
                role,
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_caller_with_neither_read_capability_is_denied() {
        let principal = agent_bearer_with(Capability::McpConnect);
        let result =
            ViewAgentsTool::REQUIRED.check(Some(&principal), &conexus_auth::NoPolicyOverrides);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn lists_live_agents_sorted_by_id_excluding_terminated_tombstone_and_system() {
        let conn = setup().await;
        {
            let c = conn.lock().await;
            seed(&c, "zebra-agent", "worker");
            seed(&c, "alpha-agent", "manager");
            seed(&c, "gone-agent", "worker");
            c.execute(
                "UPDATE agents SET status = 'terminated' WHERE agent_id = 'gone-agent'",
                [],
            )
            .unwrap();
            seed(&c, "system", "worker");
            c.execute(
                "UPDATE agents SET status = 'system' WHERE agent_id = 'system'",
                [],
            )
            .unwrap();
        }
        let principal = agent_bearer_with(Capability::AgentsUse);
        let registry = WaiterRegistry::new();
        let file_map = conexus_wakeloop::file_map::FileMap::new();
        let sea_orm_db = test_sea_orm_db().await;
        let ctx = ToolCallContext::off_wire(
            &registry,
            &file_map,
            std::path::Path::new("/tmp"),
            &sea_orm_db,
        );
        let result = ViewAgentsTool::call(
            Some(&principal),
            &Value::Null,
            &conn,
            "2026-06-01T00:00:00Z",
            &ctx,
        )
        .await;
        let ToolResult::Ok { data, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        let agents = data.unwrap()["agents"].as_array().unwrap().clone();
        let ids: Vec<&str> = agents
            .iter()
            .map(|a| a["agent_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["alpha-agent", "zebra-agent"]);
        assert_eq!(agents[0]["agent_role"], "manager");
    }

    #[tokio::test]
    async fn an_operator_tier_view_capability_also_admits() {
        let conn = setup().await;
        {
            let c = conn.lock().await;
            seed(&c, "only-agent", "worker");
        }
        let mut principal = agent_bearer_with(Capability::AgentsView);
        principal.kind = PrincipalKind::ForwardingHeader;
        principal.agent_id = None;
        let registry = WaiterRegistry::new();
        let file_map = conexus_wakeloop::file_map::FileMap::new();
        let sea_orm_db = test_sea_orm_db().await;
        let ctx = ToolCallContext::off_wire(
            &registry,
            &file_map,
            std::path::Path::new("/tmp"),
            &sea_orm_db,
        );
        let result = ViewAgentsTool::call(
            Some(&principal),
            &Value::Null,
            &conn,
            "2026-06-01T00:00:00Z",
            &ctx,
        )
        .await;
        let ToolResult::Ok { data, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert_eq!(data.unwrap()["agents"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_missing_principal_is_denied() {
        assert!(!principal_can_view_roster(None));
    }

    // --- update_agent_profile --------------------------------------
    //
    // Needs both the rusqlite `conn` (target-role lookup) and
    // `ctx.sea_orm_db` (`review_profile`) to see the same rows -- the
    // plain in-memory `test_sea_orm_db`/`setup` pair above are two
    // separate SQLite instances, so these tests use a shared tempfile
    // DB instead (same precedent as `rag_tools::tests::test_sea_orm_db`).
    async fn shared_db() -> (AsyncMutex<Connection>, sea_orm::DatabaseConnection) {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("test.db");
        {
            let c = Connection::open(&path).unwrap();
            init_schema(&c).unwrap();
        }
        let conn = AsyncMutex::new(Connection::open(&path).unwrap());
        let sea_orm_db = sea_orm::Database::connect(format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        (conn, sea_orm_db)
    }

    fn ctx_for<'a>(
        registry: &'a WaiterRegistry,
        file_map: &'a conexus_wakeloop::file_map::FileMap,
        sea_orm_db: &'a sea_orm::DatabaseConnection,
    ) -> conexus_auth::ToolCallContext<'a> {
        ToolCallContext::off_wire(registry, file_map, std::path::Path::new("/tmp"), sea_orm_db)
    }

    fn worker_bearer(agent_id: &str) -> Principal {
        Principal {
            kind: PrincipalKind::AgentBearer,
            user_id: None,
            agent_id: Some(agent_id.to_string()),
            project_name: None,
            project_role: None,
            agent_role: Some(conexus_core::capability::AgentRole::Worker),
            can_wake_loop: true,
            source_token: None,
            capabilities: Capabilities::Set(HashSet::from([Capability::AgentsUse])),
        }
    }

    fn manager_bearer(agent_id: &str) -> Principal {
        let mut p = worker_bearer(agent_id);
        p.agent_role = Some(conexus_core::capability::AgentRole::Manager);
        p
    }

    #[tokio::test]
    async fn self_confirm_with_no_profile_arg_always_succeeds_even_with_toggle_off() {
        let (conn, sea_orm_db) = shared_db().await;
        {
            let c = conn.lock().await;
            seed(&c, "worker-1", "worker");
        }
        project_settings_repository::upsert(
            &sea_orm_db,
            "config_allow_worker_update_own_profile",
            "false",
            None,
            false,
            "test",
            "2026-06-01T00:00:00Z",
        )
        .await
        .unwrap();
        let registry = WaiterRegistry::new();
        let file_map = conexus_wakeloop::file_map::FileMap::new();
        let ctx = ctx_for(&registry, &file_map, &sea_orm_db);
        let principal = worker_bearer("worker-1");
        let result = UpdateAgentProfileTool::call(
            Some(&principal),
            &Value::Null,
            &conn,
            "2026-06-01T00:00:00Z",
            &ctx,
        )
        .await;
        assert!(matches!(result, ToolResult::Ok { .. }), "{result:?}");
    }

    #[tokio::test]
    async fn self_edit_is_denied_when_the_own_role_toggle_is_off() {
        let (conn, sea_orm_db) = shared_db().await;
        {
            let c = conn.lock().await;
            seed(&c, "worker-1", "worker");
        }
        project_settings_repository::upsert(
            &sea_orm_db,
            "config_allow_worker_update_own_profile",
            "false",
            None,
            false,
            "test",
            "2026-06-01T00:00:00Z",
        )
        .await
        .unwrap();
        let registry = WaiterRegistry::new();
        let file_map = conexus_wakeloop::file_map::FileMap::new();
        let ctx = ctx_for(&registry, &file_map, &sea_orm_db);
        let principal = worker_bearer("worker-1");
        let result = UpdateAgentProfileTool::call(
            Some(&principal),
            &serde_json::json!({"profile": "new text"}),
            &conn,
            "2026-06-01T00:00:00Z",
            &ctx,
        )
        .await;
        assert!(matches!(result, ToolResult::PermissionDenied { .. }), "{result:?}");
    }

    #[tokio::test]
    async fn manager_may_curate_a_workers_profile() {
        let (conn, sea_orm_db) = shared_db().await;
        {
            let c = conn.lock().await;
            seed(&c, "manager-1", "manager");
            seed(&c, "worker-1", "worker");
        }
        let registry = WaiterRegistry::new();
        let file_map = conexus_wakeloop::file_map::FileMap::new();
        let ctx = ctx_for(&registry, &file_map, &sea_orm_db);
        let principal = manager_bearer("manager-1");
        let result = UpdateAgentProfileTool::call(
            Some(&principal),
            &serde_json::json!({"profile": "curated", "agent_id": "worker-1"}),
            &conn,
            "2026-06-01T00:00:00Z",
            &ctx,
        )
        .await;
        let ToolResult::Ok { data, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert_eq!(data.unwrap()["profile"], "curated");
    }

    #[tokio::test]
    async fn manager_may_not_edit_another_managers_profile() {
        let (conn, sea_orm_db) = shared_db().await;
        {
            let c = conn.lock().await;
            seed(&c, "manager-1", "manager");
            seed(&c, "manager-2", "manager");
        }
        let registry = WaiterRegistry::new();
        let file_map = conexus_wakeloop::file_map::FileMap::new();
        let ctx = ctx_for(&registry, &file_map, &sea_orm_db);
        let principal = manager_bearer("manager-1");
        let result = UpdateAgentProfileTool::call(
            Some(&principal),
            &serde_json::json!({"profile": "hijacked", "agent_id": "manager-2"}),
            &conn,
            "2026-06-01T00:00:00Z",
            &ctx,
        )
        .await;
        assert!(matches!(result, ToolResult::PermissionDenied { .. }), "{result:?}");
    }

    #[tokio::test]
    async fn a_worker_may_not_edit_another_agents_profile() {
        let (conn, sea_orm_db) = shared_db().await;
        {
            let c = conn.lock().await;
            seed(&c, "worker-1", "worker");
            seed(&c, "worker-2", "worker");
        }
        let registry = WaiterRegistry::new();
        let file_map = conexus_wakeloop::file_map::FileMap::new();
        let ctx = ctx_for(&registry, &file_map, &sea_orm_db);
        let principal = worker_bearer("worker-1");
        let result = UpdateAgentProfileTool::call(
            Some(&principal),
            &serde_json::json!({"profile": "hijacked", "agent_id": "worker-2"}),
            &conn,
            "2026-06-01T00:00:00Z",
            &ctx,
        )
        .await;
        assert!(matches!(result, ToolResult::PermissionDenied { .. }), "{result:?}");
    }
}

