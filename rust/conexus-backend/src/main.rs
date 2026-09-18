//! CoNexus per-project MCP backend (Phase D1 step 3).
//!
//! Boot sequence + CLI flag surface are a faithful port of `conexus/
//! cli.py`'s `server` command + `conexus/app/server_lifecycle.py`'s
//! steps 1-2 -- the exact invocation contract
//! `nix/packages.nix`'s wrapper already generates for the Python
//! binary (`--uds <sock> --project-dir <path> --forwarding-hmac-in
//! <path> --no-tui`, with `--transport sse` always added) so a
//! `backend_impl` flip needs no wrapper changes on either side.
//!
//! NOT yet ported in this first slice (see the migration plan's Phase
//! D1 "Next step" tracking): loading agent/task state into memory at
//! boot (nothing in `conexus-tools` needs an in-memory mirror the way
//! Python's caches do -- every repository call reads the DB directly),
//! `--debug`/`--advanced`/`--no-index` (RAG-indexing flags, Phase D2
//! territory), and `--port`/host:port serving (this binary only
//! serves over `--uds`, matching every REAL deployment path -- the
//! host:port fallback is a local-dev convenience Python's CLI offers
//! that has no `conexus@<name>.service` caller to replicate for yet).

mod auth_gate;
mod background_tasks;
mod boot;
mod delivery_gate;
mod delivery_transport;
mod instructions;
mod json_sanitize;
mod operator_events;
mod principal_resolve;
mod read_limits;
mod rest_gate;
mod rest_handlers;
mod rest_principal;
mod server;
mod uds;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::middleware;
use axum::routing::{get, patch, post, put};
use axum::Router;
use clap::Parser;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};

use server::{ConexusServer, SharedState};

/// `conexus server`'s flag surface, the subset this binary actually
/// serves (see the module doc for what's deliberately not ported yet).
#[derive(Parser, Debug)]
#[command(name = "conexus-backend")]
struct Cli {
    /// Unix domain socket path to listen on.
    #[arg(long)]
    uds: PathBuf,

    /// Transport type for MCP communication. Only "sse" (Streamable
    /// HTTP) is implemented -- accepted as a flag for CLI-surface
    /// compatibility with the wrapper that always passes it; any
    /// other value is a hard error.
    #[arg(long, default_value = "sse")]
    transport: String,

    /// Project directory. The `.agent` folder is created/used here.
    #[arg(long)]
    project_dir: PathBuf,

    /// Read the per-project HMAC key (raw bytes) for verifying the
    /// router's signed forwarding header.
    #[arg(long)]
    forwarding_hmac_in: Option<PathBuf>,

    /// Accepted for CLI-surface compatibility with the wrapper; this
    /// binary is always headless (no TUI exists to disable).
    #[arg(long)]
    no_tui: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.transport != "sse" {
        anyhow::bail!(
            "conexus-backend only implements the \"sse\" (Streamable HTTP) transport, got {:?}",
            cli.transport
        );
    }

    // Must run before ANY connection in this process opens -- see this
    // fn's own doc for the real, previously-live gap it closes.
    boot::register_vector_extension();

    boot::ensure_project_dirs(&cli.project_dir)?;
    let conn = boot::open_and_init_db(&cli.project_dir)?;
    // Phase G (sea-orm migration): a second, sea-orm-flavored handle
    // onto the SAME SQLite file `conn` above just opened/initialized --
    // see `SharedState::sea_orm_db`'s own doc for why this coexists
    // with the legacy rusqlite connection rather than replacing it.
    // Opened AFTER `open_and_init_db` so the schema already exists (this
    // exact rusqlite-creates-then-sea-orm-connects sequencing is the
    // same one `conexus_db::task_comments_repository`'s own tests
    // already prove works, see its `conn_with_task` test helper).
    let sea_orm_db_path = boot::db_path(&cli.project_dir);
    let sea_orm_db = sea_orm::Database::connect(format!("sqlite://{}", sea_orm_db_path.display()))
        .await
        .with_context(|| {
            format!(
                "open sea-orm connection to project database {}",
                sea_orm_db_path.display()
            )
        })?;
    // Phase F schema-authority cutover: sea-orm-migration's `Migrator`
    // is now the boot-time schema authority (replacing Alembic for a
    // real production database, and `schema::init_schema`'s own
    // already-behind DDL for a genuinely fresh one) -- a no-op against
    // this project's real database, already adopted via
    // `conexus-cli seed-baseline`.
    boot::apply_baseline_migration(&sea_orm_db)
        .await
        .with_context(|| {
            format!(
                "apply schema-authority baseline at {}",
                sea_orm_db_path.display()
            )
        })?;
    let forwarding_hmac_key = boot::load_forwarding_hmac_key(cli.forwarding_hmac_in.as_deref());
    if cli.forwarding_hmac_in.is_some() && forwarding_hmac_key.is_none() {
        eprintln!(
            "conexus-backend: forwarding-hmac key at {:?} was unreadable or empty -- \
             forwarding-header auth stays dormant",
            cli.forwarding_hmac_in
        );
    }

    let shared = Arc::new(SharedState {
        conn: tokio::sync::Mutex::new(conn),
        forwarding_hmac_key,
        waiter_registry: conexus_wakeloop::waiter_registry::WaiterRegistry::new(),
        file_map: conexus_wakeloop::file_map::FileMap::new(),
        project_dir: cli.project_dir.clone(),
        operator_events: operator_events::OperatorEventsHub::new(),
        delivery_transport: delivery_transport::DeliveryTransportHub::new(),
        sea_orm_db,
    });

    background_tasks::spawn_all(&shared);

    let shared_for_factory = shared.clone();
    let mcp_service: StreamableHttpService<ConexusServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(ConexusServer::new(shared_for_factory.clone())),
            Arc::new(LocalSessionManager::default()),
            // DNS-rebinding `Host` validation exists to protect a TCP
            // listener reachable from a browser's cross-origin
            // request; a Unix socket, gated by filesystem permissions
            // to this uid alone, isn't reachable that way at all --
            // the threat model this default guards against doesn't
            // apply here (unlike pikvm_mcp_server/m365-bridge, which
            // widen the *list* because they DO bind TCP).
            StreamableHttpServerConfig::default().disable_allowed_hosts(),
        );

    let mcp_router =
        Router::new()
            .nest_service("/mcp", mcp_service)
            .layer(middleware::from_fn_with_state(
                shared.clone(),
                auth_gate::require_identity,
            ));
    let api_router = build_api_router(shared.clone());
    let app = Router::new()
        .merge(mcp_router)
        .nest("/api", api_router)
        .with_state(shared.clone());

    uds::serve_router_unix(&cli.uds, app)
        .await
        .with_context(|| format!("serve {}", cli.uds.display()))
}

/// Builds the `/api` sub-router: three tiers (`api_public`,
/// `api_authenticated`, `api_delivery`) merged together. Factored out of
/// `main()` so `mod tests` below can bind the SAME route-to-tier
/// assignment production actually serves, over a real UDS + auth
/// middleware, rather than a hand-copied mirror of this table that could
/// silently drift from it (a route moved to the wrong tier here would go
/// unnoticed by a test built against yesterday's copy).
///
/// Split into `api_public`/`api_authenticated` so a small, explicit set
/// of genuinely-safe-to-be-anonymous endpoints can bypass `rest_gate`
/// WITHOUT weakening the default for every other route.
/// `GET /prompts/catalog` is the only route left there: it serves a
/// static, non-per-tenant prompt catalog with no project data in it.
/// `GET /tasks` and `GET /agents` used to live here too (ported
/// verbatim from a legacy Python docstring admitting "the router-level
/// gate is deferred to a follow-up PR") -- confirmed by a pentest pass
/// to leak real task free-text (title/description/notes, commonly
/// containing credentials and internal URLs) and agent-fleet metadata
/// to anonymous callers, and to work as a project-name existence oracle
/// via 404-vs-200. Moved to `api_authenticated`; this was the follow-up
/// PR. `api_authenticated`'s fallback makes "require auth" the default
/// for anything not explicitly listed on `api_public` -- a route added
/// to the wrong router is a 401 to fix, never a silent open door.
///
/// An explicit `.fallback()` is required on `api_authenticated`, not
/// cosmetic: an otherwise-empty `Router` contributes NO matchable path
/// at all when `.nest()`ed, so unmatched `/api/*` requests would fall
/// through to the OUTER router's fallback instead -- which
/// `Router::merge` had already set to `mcp_router`'s own
/// auth_gate-wrapped one in PR 1, silently routing every `/api/*`
/// request through `/mcp`'s (wrong, wider) door. Caught live during PR
/// 1: an unauthenticated `/api/*` probe returned `/mcp`'s JSON-RPC
/// error shape, and a WORKER bearer (rejected by `rest_gate`, admitted
/// by `auth_gate`) was let through -- both pointed at the same root
/// cause before this fallback was added.
fn build_api_router(shared: Arc<SharedState>) -> Router<Arc<SharedState>> {
    async fn api_not_found() -> axum::http::StatusCode {
        axum::http::StatusCode::NOT_FOUND
    }
    let api_public = Router::new().route("/prompts/catalog", get(rest_handlers::prompts_catalog));
    let api_authenticated = Router::new()
        .route("/settings-schema", get(rest_handlers::settings_schema))
        .route("/memories", post(rest_handlers::create_memory))
        .route(
            "/memories/{*context_key}",
            put(rest_handlers::update_memory).delete(rest_handlers::delete_memory),
        )
        .route(
            "/schedules",
            get(rest_handlers::list_schedules).post(rest_handlers::create_schedule),
        )
        .route(
            "/schedules/{directive_id}",
            put(rest_handlers::update_schedule).delete(rest_handlers::delete_schedule),
        )
        .route(
            "/tasks",
            get(rest_handlers::list_tasks).post(rest_handlers::create_task),
        )
        .route(
            "/tasks/{task_id}/delete-preview",
            get(rest_handlers::task_delete_preview),
        )
        .route(
            "/tasks/{task_id}",
            axum::routing::delete(rest_handlers::delete_task),
        )
        .route("/tokens", get(rest_handlers::tokens))
        .route("/settings-data", get(rest_handlers::settings_data))
        .route("/settings", post(rest_handlers::create_setting))
        .route(
            "/settings/{context_key}",
            put(rest_handlers::update_setting).delete(rest_handlers::delete_setting),
        )
        .route("/status", get(rest_handlers::simple_status))
        .route("/context-data", get(rest_handlers::context_data))
        .route(
            "/terminate-agent",
            post(rest_handlers::terminate_agent_dashboard),
        )
        .route(
            "/update-task-dashboard",
            post(rest_handlers::update_task_dashboard),
        )
        .route(
            "/create-sample-memories",
            post(rest_handlers::create_sample_memories),
        )
        .route("/all-data", get(rest_handlers::all_data))
        .route("/messages/query", post(rest_handlers::list_messages))
        .route(
            "/messages/participants",
            post(rest_handlers::list_participants),
        )
        .route(
            "/messages/suggest-subject",
            post(rest_handlers::suggest_subject),
        )
        .route("/messages", post(rest_handlers::create_message))
        .route(
            "/messages/{message_id}/thread",
            get(rest_handlers::get_message_thread),
        )
        .route(
            "/messages/{message_id}",
            patch(rest_handlers::patch_message).delete(rest_handlers::delete_message),
        )
        .route("/agents", get(rest_handlers::list_agents_dashboard))
        .route(
            "/agents/register",
            post(rest_handlers::register_agent_dashboard),
        )
        .route(
            "/agents/{agent_id}/restore",
            post(rest_handlers::restore_agent),
        )
        .route("/agents/{agent_id}/edit", post(rest_handlers::edit_agent))
        .route(
            "/agents/{agent_id}/rotate-token",
            post(rest_handlers::rotate_agent_token),
        )
        .route(
            "/agents/{agent_id}/purge-preview",
            get(rest_handlers::agent_purge_preview),
        )
        .route(
            "/agents/{agent_id}",
            axum::routing::delete(rest_handlers::purge_agent),
        )
        .route(
            "/agents/disconnect-all",
            post(rest_handlers::disconnect_all_agents),
        )
        .route(
            "/agents/reconnect-all",
            post(rest_handlers::reconnect_all_agents),
        )
        .route(
            "/agents/{agent_id}/disconnect",
            post(rest_handlers::disconnect_agent),
        )
        .route(
            "/agents/{agent_id}/reconnect",
            post(rest_handlers::reconnect_agent),
        )
        .route(
            "/agents/{agent_id}/directive",
            post(rest_handlers::poke_agent_directive),
        )
        .route("/events", get(rest_handlers::operator_events_stream))
        .route("/events/status", get(rest_handlers::operator_events_status))
        .fallback(api_not_found)
        .layer(middleware::from_fn_with_state(
            shared.clone(),
            rest_gate::require_rest_identity,
        ));
    // `/api/delivery/*` (Phase E1 PR 14/14) is a THIRD `/api` admission
    // shape -- worker-bearer-authed (see `delivery_gate`'s own doc for
    // why it can't reuse `rest_gate`'s operator-tier-only door or
    // `auth_gate`'s forwarding-header-accepting one). Merging a router
    // with its own `.layer()` is safe here (each merged sub-router keeps
    // its own middleware stack; `api_authenticated`'s existing
    // `.fallback()` only fires when NO merged router's routes match at
    // all, so it doesn't shadow these two real routes).
    let api_delivery = Router::new()
        .route("/delivery/stream", get(rest_handlers::delivery_stream))
        .route("/delivery/status", post(rest_handlers::delivery_status))
        .layer(middleware::from_fn_with_state(
            shared.clone(),
            delivery_gate::require_delivery_agent_bearer,
        ));
    Router::new()
        .merge(api_public)
        .merge(api_authenticated)
        .merge(api_delivery)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same tempdir-backed, dual-connection (`rusqlite` + sea-orm, same
    /// file) `SharedState` construction every other test module in this
    /// crate duplicates rather than shares (see e.g. `rest_gate::tests::
    /// test_shared_state`) -- kept independent so no test module takes on
    /// a cross-module test-only dependency.
    async fn test_shared_state() -> Arc<SharedState> {
        let dir = tempfile::tempdir().unwrap().keep();
        let conn = rusqlite::Connection::open(dir.join("test.db")).unwrap();
        conexus_db::schema::init_schema(&conn).unwrap();
        let sea_orm_db =
            sea_orm::Database::connect(format!("sqlite://{}", dir.join("test.db").display()))
                .await
                .unwrap();
        Arc::new(SharedState {
            conn: tokio::sync::Mutex::new(conn),
            forwarding_hmac_key: None,
            waiter_registry: conexus_wakeloop::waiter_registry::WaiterRegistry::new(),
            file_map: conexus_wakeloop::file_map::FileMap::new(),
            project_dir: std::env::temp_dir(),
            operator_events: operator_events::OperatorEventsHub::new(),
            delivery_transport: delivery_transport::DeliveryTransportHub::new(),
            sea_orm_db,
        })
    }

    /// Binds `build_api_router`'s REAL output -- the exact router
    /// `main()` serves -- on a temp UDS, mirroring `rest_gate::tests::
    /// spawn_test_app`'s own real-socket approach (a handler called
    /// directly, bypassing `Router::merge`/`.nest()`/middleware entirely,
    /// would prove nothing about which tier a route actually landed on).
    async fn spawn_api_router(shared: Arc<SharedState>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("api-router-test.sock");
        let app = build_api_router(shared.clone()).with_state(shared);
        let path_for_server = socket_path.clone();
        tokio::spawn(async move {
            let _ = uds::serve_router_unix(&path_for_server, app).await;
        });
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (dir, socket_path)
    }

    /// Drive one real HTTP request over the UDS via hyper's client (same
    /// connect-and-handshake shape as `rest_gate::tests::request_status`)
    /// and return just the status code -- every test here only needs
    /// that.
    async fn request(
        socket_path: &std::path::Path,
        method: &str,
        uri: &str,
        header: Option<(&str, &str)>,
    ) -> u16 {
        let stream = tokio::net::UnixStream::connect(socket_path).await.unwrap();
        let io = hyper_util::rt::TokioIo::new(stream);
        let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut builder = hyper::Request::builder().method(method).uri(uri);
        if let Some((name, value)) = header {
            builder = builder.header(name, value);
        }
        let request = builder.body(axum::body::Body::empty()).unwrap();
        let response = sender.send_request(request).await.unwrap();
        response.status().as_u16()
    }

    #[tokio::test]
    async fn unauthenticated_tasks_list_is_rejected_with_401() {
        let shared = test_shared_state().await;
        let (_dir, socket_path) = spawn_api_router(shared).await;
        assert_eq!(request(&socket_path, "GET", "/tasks", None).await, 401);
    }

    #[tokio::test]
    async fn unauthenticated_agents_list_is_rejected_with_401() {
        let shared = test_shared_state().await;
        let (_dir, socket_path) = spawn_api_router(shared).await;
        assert_eq!(request(&socket_path, "GET", "/agents", None).await, 401);
    }

    /// The public tier is not collateral damage: `/prompts/catalog`
    /// (confirmed-safe, static, non-per-tenant -- out of scope for this
    /// fix) must stay reachable with no auth at all.
    #[tokio::test]
    async fn unauthenticated_prompts_catalog_still_returns_200() {
        let shared = test_shared_state().await;
        let (_dir, socket_path) = spawn_api_router(shared).await;
        assert_eq!(
            request(&socket_path, "GET", "/prompts/catalog", None).await,
            200
        );
    }

    async fn insert_manager_agent(shared: &Arc<SharedState>) {
        let guard = shared.conn.lock().await;
        guard
            .execute(
                "INSERT INTO agents (token, agent_id, created_at, status, \
                 working_directory, agent_role) VALUES \
                 ('mgr-tok', 'manager', '2026-01-01T00:00:00Z', 'active', '/tmp', 'manager')",
                [],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn authenticated_operator_bearer_can_list_tasks() {
        let shared = test_shared_state().await;
        insert_manager_agent(&shared).await;
        let (_dir, socket_path) = spawn_api_router(shared).await;
        assert_eq!(
            request(
                &socket_path,
                "GET",
                "/tasks",
                Some(("Authorization", "Bearer mgr-tok")),
            )
            .await,
            200
        );
    }

    #[tokio::test]
    async fn authenticated_operator_bearer_can_list_agents() {
        let shared = test_shared_state().await;
        insert_manager_agent(&shared).await;
        let (_dir, socket_path) = spawn_api_router(shared).await;
        assert_eq!(
            request(
                &socket_path,
                "GET",
                "/agents",
                Some(("Authorization", "Bearer mgr-tok")),
            )
            .await,
            200
        );
    }
}
