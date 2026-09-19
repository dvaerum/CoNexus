//! Delivery scheduler -- drives the fallback policy and pushes frames
//! (ADR-0021), plus the `directive.due` trigger (ADR-0026). Port of
//! `conexus/features/delivery_scheduler.py`.
//!
//! Ties the pieces together: read the per-project policy config, read
//! each connected worker's signals (unread messages / open tasks /
//! unassigned tasks + reported transport-status), run the pure
//! [`crate::delivery_policy`] engine, and -- when it says ping -- render
//! a **skinny** frame (ids/titles/status, never bodies) and push it
//! down the worker's delivery stream. A second, additive per-tick step
//! (ADR-0026) fires every due scheduled directive for each connected
//! worker and pushes each as a `directive_due` frame, closing the gap
//! where a connected-but-never-polling session's schedules were only
//! ever evaluated from inside a live `wait_for_events` call.
//!
//! Per-worker bookkeeping (backoff/cooldown state) lives in this
//! process (see [`SchedulerState`]); a frame that can't be delivered
//! (no live stream) is simply dropped and the policy re-fires next tick
//! (self-healing, ADR-0021).
//!
//! ## The ADR-0026 disconnect race, re-derived for Rust (not assumed)
//!
//! Python's `_fire_due_directives` argues its check-then-fire-then-push
//! sequence is race-free because nothing else can run between the
//! `connected_agent_ids()` check and `push()` -- true only under
//! single-threaded cooperative asyncio with no `await` in that chain.
//! This port's equivalent chain (`tick` -> `fire_due_directives` ->
//! `collect_due_and_fire().await` -> `push()`) has a REAL `.await` in
//! the middle, on tokio's real multi-threaded runtime: a concurrent
//! `unsubscribe` genuinely COULD interleave between the connectivity
//! check that selected this agent and the `push()` call. This is
//! accepted as harmless, not closed with an extra lock, because
//! `DeliveryTransportHub::push`'s own contract already treats "no live
//! subscriber" as a normal, silently-dropped outcome (it returns `0`,
//! never errors) -- the exact same self-healing contract every other
//! trigger in this module already relies on for a mid-tick disconnect.
//! The one behavior this residual race can produce that Python's design
//! could never see -- a directive fired (mutated, `run_count`
//! incremented) but not delivered because the worker disconnected in
//! the `.await` gap -- is indistinguishable from the *already-accepted*
//! "genuinely full subscriber queue" case ADR-0026 itself calls "an
//! extremely rare, self-inflicted edge case, not a routine occurrence":
//! both are a fired-but-undelivered directive, and both self-heal the
//! same way a missed `evaluate_and_push` does -- not at all, since
//! `collect_due_and_fire` is non-self-healing by design (ADR-0026's own
//! documented tradeoff), but re-connecting re-arms the `wait_for_events`
//! path for anything still due. No additional lock is taken across the
//! check+fire+push span.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::delivery_policy::{
    self as dp, DeliveryPolicyConfig, PingBookkeeping, Reason, TransportStatus, WorkerSignals,
};
use crate::server::SharedState;

/// Per-worker ping bookkeeping for THIS backend process. A real
/// `Mutex<HashMap<...>>`, not Python's unlocked module-level dict --
/// Python's GIL-implicit safety (single-threaded asyncio, no `await`
/// between `_bookkeeping.get(...)` and `_bookkeeping[agent_id] = ...`)
/// does not carry over to Rust's real multi-threaded tokio runtime,
/// where two concurrent callers (a live `tick()` racing a direct
/// `evaluate_and_push` call, e.g. from a test or another future
/// caller) could genuinely interleave a read-then-write on the same
/// `agent_id`. [`SchedulerState::evaluate`] holds the lock across the
/// whole decide-then-store step to close that window -- the same
/// `Mutex<HashMap<String, _>>` shape [`conexus_wakeloop::
/// waiter_registry::WaiterRegistry`] and [`conexus_wakeloop::
/// hold_ladder`] already use for this exact class of per-agent
/// server-side state.
#[derive(Default)]
pub struct SchedulerState {
    bookkeeping: Mutex<HashMap<String, PingBookkeeping>>,
}

impl SchedulerState {
    pub fn new() -> Self {
        Self::default()
    }

    fn evaluate(
        &self,
        agent_id: &str,
        config: &DeliveryPolicyConfig,
        signals: &WorkerSignals,
        now: i64,
    ) -> dp::PingDecision {
        let mut map = self
            .bookkeeping
            .lock()
            .expect("delivery_scheduler bookkeeping mutex poisoned");
        let bk = map.get(agent_id).copied().unwrap_or_default();
        let decision = dp::evaluate(config, signals, bk, now);
        map.insert(agent_id.to_string(), decision.bookkeeping);
        decision
    }

    /// Test hook -- reset per-worker bookkeeping. Port of Python's
    /// `clear()`.
    #[cfg(test)]
    fn clear(&self) {
        self.bookkeeping
            .lock()
            .expect("delivery_scheduler bookkeeping mutex poisoned")
            .clear();
    }
}

/// How often the background loop re-evaluates (a poll floor; the
/// policy's own backoff decides whether a tick actually pings). Matches
/// Python's `TICK_INTERVAL_SECONDS`.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);

/// `_count_unassigned_open`'s own local terminal-status set -- includes
/// BOTH `cancelled` and `canceled` spellings. A separate list from
/// every other terminal-status constant in this codebase (ported
/// verbatim; Python's own source hardcodes this same separate tuple
/// rather than sharing one with e.g. `resources/status.py`'s).
const UNASSIGNED_TERMINAL_STATUSES: &[&str] = &["completed", "cancelled", "canceled", "failed"];

/// Far enough in the past that `list_unassigned_active_updated_since`
/// returns every unassigned open task project-wide -- Python's own scan
/// has no time filter at all. Matches this codebase's own
/// `background_tasks::rag_indexing` epoch-watermark convention.
const EPOCH_ISO: &str = "1970-01-01T00:00:00Z";

/// Mirrors Python's own choice of `time.monotonic()` over wall-clock
/// time for backoff timing -- an NTP step must never suddenly satisfy
/// (or extend) a worker's backoff window. `OnceLock`-cached process-
/// start `Instant`, so [`monotonic_now`] returns whole seconds elapsed
/// since this process booted: an arbitrary reference point, exactly
/// like `time.monotonic()`'s own contract (only differences between
/// two calls are meaningful, never the absolute value).
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

pub fn monotonic_now() -> i64 {
    let start = PROCESS_START.get_or_init(Instant::now);
    start.elapsed().as_secs() as i64
}

/// Resolve the per-project fallback policy from `project_settings`.
/// Same defaults as `conexus-core`'s `settings_schema.rs` entries.
pub async fn load_config(db: &sea_orm::DatabaseConnection) -> DeliveryPolicyConfig {
    use conexus_db::project_settings_repository::{get_bool, get_int};
    DeliveryPolicyConfig {
        enabled: get_bool(db, "config_delivery_enabled", false).await,
        on_unread_messages: get_bool(db, "config_delivery_on_unread_messages", true).await,
        on_unfinished_tasks: get_bool(db, "config_delivery_on_unfinished_tasks", true).await,
        on_unassigned_tasks: get_bool(db, "config_delivery_on_unassigned_tasks", false).await,
        on_due_directives: get_bool(db, "config_delivery_on_due_directives", true).await,
        backoff_initial_seconds: get_int(db, "config_delivery_backoff_initial_seconds", 30).await,
        backoff_max_seconds: get_int(db, "config_delivery_backoff_max_seconds", 3600).await,
        cooldown_seconds: get_int(db, "config_delivery_cooldown_seconds", 60).await,
        wake_dormant: get_bool(db, "config_delivery_wake_dormant", false).await,
    }
}

/// Open tasks in the pool with no assignee (project-wide). Port of
/// `_count_unassigned_open` -- a DB failure degrades to `0`, matching
/// Python's own defensive `except Exception: return 0`.
async fn count_unassigned_open(db: &sea_orm::DatabaseConnection) -> i64 {
    match conexus_db::task_repository::list_unassigned_active_updated_since(
        db,
        EPOCH_ISO,
        UNASSIGNED_TERMINAL_STATUSES,
    )
    .await
    {
        Ok(rows) => rows.len() as i64,
        Err(_) => 0,
    }
}

async fn signals_for(
    sea_orm_db: &sea_orm::DatabaseConnection,
    transport: &crate::delivery_transport::DeliveryTransportHub,
    agent_id: &str,
    backlog: &Option<conexus_wakeloop::idle_reminder::Backlog>,
    config: &DeliveryPolicyConfig,
) -> WorkerSignals {
    let unread = backlog.as_ref().map(|b| b.unread_count).unwrap_or(0);
    let open_tasks = backlog.as_ref().map(|b| b.task_count as i64).unwrap_or(0);
    // Only pay the project-wide scan when the trigger is armed.
    let unassigned = if config.on_unassigned_tasks {
        count_unassigned_open(sea_orm_db).await
    } else {
        0
    };
    // A connected worker that hasn't reported status yet is treated as
    // idle (deliverable); an explicit report overrides.
    let status = TransportStatus::from_report(transport.get_status(agent_id).as_deref());
    WorkerSignals {
        unread_messages: unread,
        open_tasks,
        unassigned_tasks: unassigned,
        transport_status: status,
    }
}

/// A SKINNY frame -- ids/subjects/status only, never message bodies
/// (ADR-0021). Mirrors what the event loop would have delivered.
fn render_frame(
    reason: Reason,
    backlog: &Option<conexus_wakeloop::idle_reminder::Backlog>,
    unassigned_count: i64,
) -> Value {
    let (unread_count, task_count, unread_messages, open_tasks) = match backlog {
        Some(b) => (
            b.unread_count,
            b.task_count as i64,
            b.unread_messages
                .iter()
                .map(|m| {
                    json!({
                        "message_id": m.message_id,
                        "sender_id": m.sender_id,
                        "subject": m.subject,
                    })
                })
                .collect::<Vec<_>>(),
            b.open_tasks
                .iter()
                .map(|t| {
                    json!({
                        "task_id": t.task_id,
                        "title": t.title,
                        "status": t.status,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        None => (0, 0, Vec::new(), Vec::new()),
    };
    let mut frame = json!({
        "type": "delivery",
        "reason": reason.as_str(),
        "unread_count": unread_count,
        "task_count": task_count,
        "unread_messages": unread_messages,
        "open_tasks": open_tasks,
    });
    if reason == Reason::UnassignedTasks {
        frame["unassigned_count"] = json!(unassigned_count);
    }
    frame
}

/// A `directive.due` delivery frame (ADR-0026) -- wraps the SAME event
/// shape `scheduled_directive_repository::collect_due_and_fire` already
/// emits for the `wait_for_events` path, unmodified, so a downstream
/// consumer sees identical directive content regardless of which path
/// delivered it.
///
/// No skinny-redaction here (unlike [`render_frame`]): a directive's
/// `data.prompt` is first-party content the agent/operator itself
/// authored, not a third party's message body -- ADR-0021's "never ship
/// bodies" concern doesn't apply the same way (documented asymmetry,
/// ADR-0026).
fn render_directive_frame(
    event: conexus_db::pending_directive_repository::DirectiveEvent,
) -> Value {
    json!({
        "type": "delivery",
        "reason": "directive_due",
        "directive": serde_json::to_value(event)
            .expect("DirectiveEvent always serializes to a JSON object"),
    })
}

/// Evaluate one worker and push a frame iff the policy says ping.
/// Returns whether a frame was pushed. Advances the worker's
/// bookkeeping.
pub async fn evaluate_and_push(
    shared: &Arc<SharedState>,
    agent_id: &str,
    config: &DeliveryPolicyConfig,
    now: i64,
) -> bool {
    let backlog = conexus_wakeloop::idle_reminder::collect_backlog(
        &shared.conn,
        &shared.sea_orm_db,
        agent_id,
    )
    .await;
    let signals = signals_for(
        &shared.sea_orm_db,
        &shared.delivery_transport,
        agent_id,
        &backlog,
        config,
    )
    .await;
    let decision = shared
        .delivery_scheduler
        .evaluate(agent_id, config, &signals, now);
    if !decision.should_ping {
        return false;
    }
    let reason = decision
        .reason
        .expect("should_ping is only ever true alongside an active reason");
    let frame = render_frame(reason, &backlog, signals.unassigned_tasks);
    shared.delivery_transport.push(agent_id, frame);
    true
}

/// Fire every due scheduled directive for `agent_id` and push each as a
/// `directive_due` frame. Returns whether anything was pushed.
///
/// Only called from [`tick`] for agents already in
/// `delivery_transport::connected_agent_ids()` -- never for a
/// disconnected worker (preserves offline-fire-once-on-reconnect).
/// `collect_due_and_fire` mutates unconditionally regardless of
/// `push()`'s outcome; see this module's own doc for the re-derived
/// disconnect-race argument covering why that's an accepted tradeoff
/// here too, not a new hazard this port introduces.
async fn fire_due_directives(shared: &Arc<SharedState>, agent_id: &str, now_iso: &str) -> bool {
    let events = match conexus_db::scheduled_directive_repository::collect_due_and_fire(
        &shared.sea_orm_db,
        agent_id,
        now_iso,
    )
    .await
    {
        Ok(events) => events,
        Err(_) => return false,
    };
    let mut pushed = false;
    for event in events {
        if shared
            .delivery_transport
            .push(agent_id, render_directive_frame(event))
            > 0
        {
            pushed = true;
        }
    }
    pushed
}

/// One scheduler pass over every connected worker. Returns the number
/// of frames pushed. A no-op (no further config read cost beyond the
/// master-switch toggle) when the feature is disabled.
pub async fn tick(shared: &Arc<SharedState>, now: i64) -> usize {
    let config = load_config(&shared.sea_orm_db).await;
    if !config.enabled {
        return 0;
    }
    let now_iso = chrono::Utc::now().to_rfc3339();
    let mut pushed = 0usize;
    for agent_id in shared.delivery_transport.connected_agent_ids() {
        // Each worker's evaluation/directive-fire is independent -- a
        // failure inside either (both already degrade to `false`
        // rather than propagate, matching Python's per-agent
        // try/except) must never stop the rest of this tick.
        if evaluate_and_push(shared, &agent_id, &config, now).await {
            pushed += 1;
        }
        if config.on_due_directives && fire_due_directives(shared, &agent_id, &now_iso).await {
            pushed += 1;
        }
    }
    pushed
}

pub async fn run_periodically(shared: Arc<SharedState>, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        let pushed = tick(&shared, monotonic_now()).await;
        if pushed > 0 {
            eprintln!("conexus-backend: delivery scheduler tick pushed {pushed} frame(s)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conexus_db::message_repository::{self, NewMessage};
    use conexus_db::schema::init_schema;
    use conexus_wakeloop::file_map::FileMap;
    use conexus_wakeloop::waiter_registry::WaiterRegistry;

    fn cfg() -> DeliveryPolicyConfig {
        DeliveryPolicyConfig {
            enabled: true,
            on_unread_messages: true,
            on_unfinished_tasks: true,
            on_unassigned_tasks: false,
            on_due_directives: true,
            backoff_initial_seconds: 30,
            backoff_max_seconds: 3600,
            cooldown_seconds: 60,
            wake_dormant: false,
        }
    }

    /// Same real-temp-file-DB-shared-between-rusqlite-and-sea-orm shape
    /// as `background_tasks.rs`'s own `test_shared()` helpers -- an
    /// in-memory `:memory:` DB can't be shared across two separate
    /// connection handles the way a real file can.
    async fn test_shared() -> (tempfile::TempDir, Arc<SharedState>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        init_schema(&conn).unwrap();
        let sea_orm_db = sea_orm::Database::connect(format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        let shared = Arc::new(SharedState {
            conn: tokio::sync::Mutex::new(conn),
            forwarding_hmac_key: None,
            waiter_registry: WaiterRegistry::new(),
            file_map: FileMap::new(),
            project_dir: std::env::temp_dir(),
            operator_events: crate::operator_events::OperatorEventsHub::new(),
            delivery_transport: crate::delivery_transport::DeliveryTransportHub::new(),
            delivery_scheduler: SchedulerState::new(),
            sea_orm_db,
        });
        (dir, shared)
    }

    fn seed_agent(conn: &rusqlite::Connection, agent_id: &str) {
        conn.execute(
            "INSERT INTO agents (token, agent_id, created_at, status, current_task, working_directory, color, agent_role) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                format!("tok-{agent_id}"),
                agent_id,
                "2026-01-01T00:00:00Z",
                "created",
                Option::<String>::None,
                "/tmp",
                Option::<String>::None,
                "worker",
            ),
        )
        .unwrap();
    }

    fn seed_message(conn: &rusqlite::Connection, id: &str, recipient_id: &str, content: &str) {
        message_repository::send(
            conn,
            NewMessage {
                message_id: id,
                sender_id: "alice_sender",
                recipient_id,
                message_content: content,
                message_type: "direct",
                priority: "normal",
                timestamp: "2026-01-01T00:00:00Z",
                delivered: true,
                read: false,
                subject: None,
                parent_message_id: None,
            },
        )
        .unwrap();
    }

    async fn set_config(db: &sea_orm::DatabaseConnection, key: &str, value: &str) {
        conexus_db::project_settings_repository::upsert(
            db,
            key,
            value,
            None,
            false,
            "test",
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
    }

    // ── evaluate_and_push ────────────────────────────────────────────

    #[tokio::test]
    async fn pushes_skinny_frame_on_unread() {
        let (_dir, shared) = test_shared().await;
        {
            let conn = shared.conn.lock().await;
            seed_agent(&conn, "bob");
            seed_message(&conn, "m1", "bob", "hello bob");
        }
        let mut sub = shared.delivery_transport.subscribe("bob");

        assert!(evaluate_and_push(&shared, "bob", &cfg(), 100).await);
        let frame = sub.receiver.try_recv().unwrap();
        assert_eq!(frame["type"], "delivery");
        assert_eq!(frame["reason"], "unread_messages");
        assert!(frame["unread_count"].as_i64().unwrap() >= 1);
        let first = &frame["unread_messages"][0];
        // SKINNY by shape: only id/sender/subject, never a body field.
        assert!(first.get("message_id").is_some());
        assert!(first.get("sender_id").is_some());
        assert!(first.get("subject").is_some());
        assert!(first.get("message").is_none());
        assert!(first.get("content").is_none());
        assert!(first.get("body").is_none());
    }

    #[tokio::test]
    async fn disabled_config_never_pushes() {
        let (_dir, shared) = test_shared().await;
        {
            let conn = shared.conn.lock().await;
            seed_agent(&conn, "bob");
            seed_message(&conn, "m1", "bob", "hi");
        }
        shared.delivery_transport.subscribe("bob");
        let disabled = DeliveryPolicyConfig {
            enabled: false,
            ..cfg()
        };
        assert!(!evaluate_and_push(&shared, "bob", &disabled, 100).await);
    }

    #[tokio::test]
    async fn working_status_suppresses_push() {
        let (_dir, shared) = test_shared().await;
        {
            let conn = shared.conn.lock().await;
            seed_agent(&conn, "bob");
            seed_message(&conn, "m1", "bob", "hi");
        }
        shared.delivery_transport.subscribe("bob");
        shared.delivery_transport.set_status("bob", "working");
        assert!(!evaluate_and_push(&shared, "bob", &cfg(), 100).await);
    }

    #[tokio::test]
    async fn no_backlog_no_push() {
        let (_dir, shared) = test_shared().await;
        {
            let conn = shared.conn.lock().await;
            seed_agent(&conn, "bob"); // no messages, no tasks
        }
        shared.delivery_transport.subscribe("bob");
        assert!(!evaluate_and_push(&shared, "bob", &cfg(), 100).await);
    }

    #[tokio::test]
    async fn backoff_prevents_double_push() {
        let (_dir, shared) = test_shared().await;
        {
            let conn = shared.conn.lock().await;
            seed_agent(&conn, "bob");
            seed_message(&conn, "m1", "bob", "hi");
        }
        shared.delivery_transport.subscribe("bob");
        let config = cfg();
        assert!(evaluate_and_push(&shared, "bob", &config, 100).await);
        // 10s later -- under the 60s cooldown -> no second push.
        assert!(!evaluate_and_push(&shared, "bob", &config, 110).await);
        // Past the cooldown -> pings again.
        assert!(evaluate_and_push(&shared, "bob", &config, 160).await);
    }

    // ── tick ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tick_reads_config_and_noops_when_disabled() {
        let (_dir, shared) = test_shared().await;
        {
            let conn = shared.conn.lock().await;
            seed_agent(&conn, "bob");
            seed_message(&conn, "m1", "bob", "hi");
        }
        shared.delivery_transport.subscribe("bob");
        assert_eq!(tick(&shared, 100).await, 0);
        assert!(!load_config(&shared.sea_orm_db).await.enabled);
    }

    // ── directive.due (ADR-0026) ─────────────────────────────────────

    async fn seed_schedule(
        shared: &Arc<SharedState>,
        agent_id: &str,
        directive_id: &str,
        next_due_at: &str,
    ) {
        {
            let conn = shared.conn.lock().await;
            seed_agent(&conn, agent_id);
        }
        conexus_db::scheduled_directive_repository::create(
            &shared.sea_orm_db,
            directive_id,
            agent_id,
            "check in with all workers",
            60,
            next_due_at,
            None,
            None,
            Some(agent_id),
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connected_but_non_polling_agent_gets_its_overdue_directive_fired() {
        let (_dir, shared) = test_shared().await;
        seed_schedule(&shared, "bob", "d1", "2020-01-01T00:00:00Z").await;
        let mut sub = shared.delivery_transport.subscribe("bob"); // connected, never polls
        set_config(&shared.sea_orm_db, "config_delivery_enabled", "true").await;

        let pushed = tick(&shared, 100).await;

        assert!(
            pushed >= 1,
            "tick() should have fired the overdue directive"
        );
        let frame = sub.receiver.try_recv().unwrap();
        assert_eq!(frame["reason"], "directive_due");
    }

    #[tokio::test]
    async fn tick_fires_due_directive_and_pushes_the_full_frame_shape() {
        let (_dir, shared) = test_shared().await;
        seed_schedule(&shared, "bob", "d1", "2020-01-01T00:00:00Z").await;
        let mut sub = shared.delivery_transport.subscribe("bob");
        set_config(&shared.sea_orm_db, "config_delivery_enabled", "true").await;

        assert!(tick(&shared, 100).await >= 1);
        let frame = sub.receiver.try_recv().unwrap();
        assert_eq!(frame["type"], "delivery");
        assert_eq!(frame["reason"], "directive_due");
        assert_eq!(
            frame["directive"]["data"]["prompt"],
            "check in with all workers"
        );
        assert_eq!(frame["directive"]["data"]["source"], "schedule");

        let row = conexus_db::scheduled_directive_repository::get(&shared.sea_orm_db, "d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.run_count, 1);
    }

    #[tokio::test]
    async fn not_due_directive_is_not_pushed() {
        let (_dir, shared) = test_shared().await;
        // Far in the future -- not due.
        seed_schedule(&shared, "bob", "d1", "2099-01-01T00:00:00Z").await;
        let mut sub = shared.delivery_transport.subscribe("bob");
        set_config(&shared.sea_orm_db, "config_delivery_enabled", "true").await;

        assert_eq!(tick(&shared, 100).await, 0);
        assert!(sub.receiver.try_recv().is_err());
        let row = conexus_db::scheduled_directive_repository::get(&shared.sea_orm_db, "d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.run_count, 0);
    }

    #[tokio::test]
    async fn disconnected_agent_directive_is_never_read() {
        // Protects offline-fire-once-on-reconnect: tick() must never
        // touch a disconnected worker's row (no live delivery stream
        // => not iterated).
        let (_dir, shared) = test_shared().await;
        seed_schedule(&shared, "bob", "d1", "2020-01-01T00:00:00Z").await;
        // No subscribe() -- worker never connects a delivery stream.
        set_config(&shared.sea_orm_db, "config_delivery_enabled", "true").await;

        assert_eq!(tick(&shared, 100).await, 0);
        let row = conexus_db::scheduled_directive_repository::get(&shared.sea_orm_db, "d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.run_count, 0);
    }

    #[tokio::test]
    async fn per_trigger_toggle_off_suppresses_the_directive_path() {
        // Master switch on, but the per-trigger toggle off: tick() must
        // not fire the due directive even though the worker is
        // connected and armed.
        let (_dir, shared) = test_shared().await;
        seed_schedule(&shared, "bob", "d1", "2020-01-01T00:00:00Z").await;
        let mut sub = shared.delivery_transport.subscribe("bob");
        set_config(&shared.sea_orm_db, "config_delivery_enabled", "true").await;
        set_config(
            &shared.sea_orm_db,
            "config_delivery_on_due_directives",
            "false",
        )
        .await;

        assert_eq!(tick(&shared, 100).await, 0);
        assert!(sub.receiver.try_recv().is_err());
        let row = conexus_db::scheduled_directive_repository::get(&shared.sea_orm_db, "d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.run_count, 0);
    }

    #[tokio::test]
    async fn master_switch_off_suppresses_the_directive_trigger_too() {
        let (_dir, shared) = test_shared().await;
        seed_schedule(&shared, "bob", "d1", "2020-01-01T00:00:00Z").await;
        let mut sub = shared.delivery_transport.subscribe("bob");
        // config_delivery_enabled defaults False -- tick() must
        // short-circuit before ever reading connected_agent_ids()/directives.
        assert!(!load_config(&shared.sea_orm_db).await.enabled);

        assert_eq!(tick(&shared, 100).await, 0);
        assert!(sub.receiver.try_recv().is_err());
        let row = conexus_db::scheduled_directive_repository::get(&shared.sea_orm_db, "d1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.run_count, 0);
    }

    // ── SchedulerState ───────────────────────────────────────────────

    #[test]
    fn clear_resets_bookkeeping_so_a_previously_armed_worker_pings_immediately_again() {
        let state = SchedulerState::new();
        let config = cfg();
        let signals = WorkerSignals {
            unread_messages: 1,
            open_tasks: 0,
            unassigned_tasks: 0,
            transport_status: TransportStatus::Idle,
        };
        let first = state.evaluate("bob", &config, &signals, 100);
        assert!(first.should_ping);
        let second = state.evaluate("bob", &config, &signals, 110);
        assert!(!second.should_ping, "still within cooldown");

        state.clear();

        let third = state.evaluate("bob", &config, &signals, 111);
        assert!(third.should_ping, "cleared bookkeeping re-arms immediately");
    }
}
