//! Port of `conexus/repositories/pending_directive_repository.py`.
//!
//! `pending_directive` is the one-shot, human-triggered "poke" queue:
//! an operator pushes a single ad-hoc directive to one agent
//! out-of-band. It is NOT recurring — no `next_due_at`/interval —
//! just one row, delivered once (stamped `delivered_at`) or sitting
//! undelivered until the next check-in. Its sibling
//! [`crate::scheduled_directive_repository`] is the same
//! delivery concept at a different lifecycle stage: a recurring,
//! self-scheduling directive. Both converge on the identical
//! `directive` event JSON shape ([`DirectiveEvent`]), distinguished
//! only by `data.source` (`"poke"` vs `"schedule"`) and
//! `data.schedule_id` (`None` vs the schedule's id) — per ADR-0026,
//! both feed the same unified delivery-scheduler push mechanism.
//!
//! A module of plain functions, matching Python's own design (no
//! cache). Every function that touches the DB takes the
//! `&DatabaseConnection` it should run against — this crate has no
//! separate "opens its own connection" path, matching every other
//! repository here. Unlike `group_capability_repository`, this table
//! lives on the per-project AGENT database (confirmed via the ORM
//! model, `conexus/db/models/pending_directive.py`), not the router
//! DB — don't assume from a sibling repository's placement.
//!
//! `agent_id` has NO database-level foreign key to `agents.agent_id`
//! — Python's model/migration comments call it a "logical FK" only,
//! so this port doesn't assume referential-integrity enforcement at
//! the DB layer either.
//!
//! Phase G (sea-orm migration): the sixth repository converted, and
//! the first whose own collector inside `assemble_event_feed`'s
//! wake-loop pipeline is now sea-orm-backed too — the prerequisite PR
//! (#975) threaded a `&sea_orm::DatabaseConnection` through that
//! function's async plumbing specifically so this conversion could
//! land without reshaping its signature again. [`poke_event`] stays a
//! pure, connection-less function throughout — it builds the wire
//! shape from already-fetched fields, nothing else in this module
//! changes that.

use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, SqliteTransactionMode, TransactionOptions, TransactionTrait,
};

pub use crate::entity::pending_directive::Model as PendingDirectiveRow;
use crate::entity::pending_directive::{ActiveModel, Column, Entity};

/// The wire shape both poke and scheduled directives converge on.
/// `event_type` serializes as `"type"` to match the JSON key Python's
/// `_poke_event`/`_schedule_event` produce.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DirectiveEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub ref_id: String,
    pub timestamp: String,
    pub priority: String,
    pub data: DirectiveEventData,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DirectiveEventData {
    pub prompt: String,
    pub source: String,
    pub schedule_id: Option<String>,
}

/// Ported from Python's `_poke_event`: `priority or "urgent"` — a
/// defensive fallback for a stored empty-string priority (the DB
/// column is `NOT NULL DEFAULT 'urgent'`, but this guards a row that
/// somehow got an empty string written directly, bypassing
/// `create_poke`'s own default). `pub` (not `pub(crate)`): Phase E1 PR
/// 14 (`conexus-rest-delivery-transport`) needs the identical wire
/// shape to build the `delivery` frame `POST /api/agents/{id}/
/// directive` pushes onto an already-connected worker's stream, the
/// same event this repository's own INSERT path constructs.
pub fn poke_event(poke_id: &str, prompt: &str, priority: &str, timestamp: &str) -> DirectiveEvent {
    let priority = if priority.is_empty() {
        "urgent"
    } else {
        priority
    };
    DirectiveEvent {
        event_type: "directive".to_string(),
        ref_id: poke_id.to_string(),
        timestamp: timestamp.to_string(),
        priority: priority.to_string(),
        data: DirectiveEventData {
            prompt: prompt.to_string(),
            source: "poke".to_string(),
            schedule_id: None,
        },
    }
}

/// INSERT an undelivered poke row. Returns the row as constructed
/// from the INSERT's own parameters — matching Python exactly, this
/// does NOT re-`SELECT` afterward. A duplicate `poke_id` surfaces as
/// a real `DbErr` (PK violation), not a special variant — matching
/// Python, which lets the underlying `sqlite3.IntegrityError`
/// propagate uncaught, and matching the rusqlite version's own
/// uncaught-`rusqlite::Error` behavior before this port.
#[allow(clippy::too_many_arguments)]
pub async fn create_poke(
    db: &DatabaseConnection,
    poke_id: &str,
    agent_id: &str,
    prompt: &str,
    priority: Option<&str>,
    created_by: Option<&str>,
    now_iso: &str,
) -> Result<PendingDirectiveRow, DbErr> {
    let priority = priority.unwrap_or("urgent");
    let am = ActiveModel {
        poke_id: Set(poke_id.to_string()),
        agent_id: Set(agent_id.to_string()),
        prompt: Set(prompt.to_string()),
        priority: Set(priority.to_string()),
        created_at: Set(now_iso.to_string()),
        created_by: Set(created_by.map(str::to_string)),
        delivered_at: Set(None),
    };
    Entity::insert(am).exec(db).await?;
    Ok(PendingDirectiveRow {
        poke_id: poke_id.to_string(),
        agent_id: agent_id.to_string(),
        prompt: prompt.to_string(),
        priority: priority.to_string(),
        created_at: now_iso.to_string(),
        created_by: created_by.map(String::from),
        delivered_at: None,
    })
}

/// Collect + mark-delivered every undelivered poke for `agent_id`.
/// Each row's `delivered_at` is stamped inside this same pass, so a
/// poke can never be double-delivered even if the caller re-invokes
/// this before committing (the caller owns the transaction/commit,
/// matching Python). Returns events in `created_at ASC` order —
/// "urgent sorts to the front" is a caller-side concern (Python's
/// `_sort_events_priority_then_time`), NOT this repository's job;
/// don't conflate SQL ordering with delivery-order guarantees.
/// Empty (never an error) when nothing is undelivered.
///
/// **F17 class-sweep fix**: this has the IDENTICAL shape as
/// [`crate::scheduled_directive_repository::collect_due_and_fire`] --
/// select into memory, per-row mark-delivered, `events.push` per row
/// -- and the SAME fix: the whole SELECT+loop+UPDATEs now runs inside
/// one `BEGIN IMMEDIATE` transaction (`TransactionTrait::
/// begin_with_options` with `SqliteTransactionMode::Immediate`, same
/// idiom as the scheduled sibling and the F9 fix in
/// `admin_group_capabilities.rs`), so a concurrent delete of a
/// candidate poke can never land in between the SELECT and its own
/// mark-delivered UPDATE. No delete/cancel-poke endpoint exists on
/// this table yet, so this was latent rather than live-exploitable --
/// fixed anyway per this project's class-sweep discipline (the bug
/// pattern here is actually *worse* than the scheduled sibling's:
/// `Entity::update_many()` never errors on zero affected rows, so a
/// race would have silently delivered a phantom event for an
/// already-gone poke with no error signal at all, unlike
/// `collect_due_and_fire`'s single-row `Entity::update()`, which at
/// least surfaces `DbErr::RecordNotUpdated`).
pub async fn collect_undelivered(
    db: &DatabaseConnection,
    agent_id: &str,
    now_iso: &str,
) -> Result<Vec<DirectiveEvent>, DbErr> {
    let tx = db
        .begin_with_options(TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        })
        .await?;

    let rows = Entity::find()
        .filter(Column::AgentId.eq(agent_id))
        .filter(Column::DeliveredAt.is_null())
        .order_by_asc(Column::CreatedAt)
        .all(&tx)
        .await?;

    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        // F17 test-only race-injection seam -- see `tests::race_hook`'s
        // doc comment. No-op (and compiled out entirely) outside tests.
        #[cfg(test)]
        tests::race_hook::pause_before_write(&row.poke_id).await;

        Entity::update_many()
            .col_expr(
                Column::DeliveredAt,
                sea_orm::sea_query::Expr::value(now_iso),
            )
            .filter(Column::PokeId.eq(row.poke_id.clone()))
            .exec(&tx)
            .await?;
        events.push(poke_event(
            &row.poke_id,
            &row.prompt,
            &row.priority,
            now_iso,
        ));
    }
    tx.commit().await?;
    Ok(events)
}

/// Count of undelivered pokes for `agent_id`. `0`, never an error,
/// when there are none (`COUNT(*)` always returns exactly one row).
pub async fn count_undelivered(db: &DatabaseConnection, agent_id: &str) -> Result<i64, DbErr> {
    let count = Entity::find()
        .filter(Column::AgentId.eq(agent_id))
        .filter(Column::DeliveredAt.is_null())
        .count(db)
        .await?;
    Ok(count as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::init_schema;
    use sea_orm::Database;

    async fn test_conn() -> (tempfile::TempDir, DatabaseConnection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            init_schema(&c).unwrap();
        }
        let db = Database::connect(format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn create_poke_returns_the_row_it_just_inserted() {
        let (_dir, db) = test_conn().await;
        let row = create_poke(
            &db,
            "poke-1",
            "alice",
            "check in",
            None,
            Some("admin"),
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        assert_eq!(row.poke_id, "poke-1");
        assert_eq!(row.agent_id, "alice");
        assert_eq!(row.prompt, "check in");
        assert_eq!(
            row.priority, "urgent",
            "default priority when None is passed"
        );
        assert_eq!(row.created_by.as_deref(), Some("admin"));
        assert_eq!(row.delivered_at, None);
    }

    #[tokio::test]
    async fn create_poke_duplicate_poke_id_is_a_real_error() {
        let (_dir, db) = test_conn().await;
        create_poke(
            &db,
            "poke-1",
            "alice",
            "first",
            None,
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let err = create_poke(
            &db,
            "poke-1",
            "alice",
            "second",
            None,
            None,
            "2026-01-01T00:00:01Z",
        )
        .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn count_undelivered_reflects_only_undelivered_rows() {
        let (_dir, db) = test_conn().await;
        assert_eq!(count_undelivered(&db, "alice").await.unwrap(), 0);
        create_poke(
            &db,
            "poke-1",
            "alice",
            "p1",
            None,
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        create_poke(
            &db,
            "poke-2",
            "alice",
            "p2",
            None,
            None,
            "2026-01-01T00:00:01Z",
        )
        .await
        .unwrap();
        assert_eq!(count_undelivered(&db, "alice").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn count_undelivered_is_scoped_per_agent() {
        let (_dir, db) = test_conn().await;
        create_poke(
            &db,
            "poke-1",
            "alice",
            "p1",
            None,
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        create_poke(
            &db,
            "poke-2",
            "bob",
            "p2",
            None,
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert_eq!(count_undelivered(&db, "alice").await.unwrap(), 1);
        assert_eq!(count_undelivered(&db, "bob").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn collect_undelivered_marks_delivered_exactly_once() {
        let (_dir, db) = test_conn().await;
        create_poke(
            &db,
            "poke-1",
            "alice",
            "check in",
            Some("high"),
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let events = collect_undelivered(&db, "alice", "2026-01-01T00:00:05Z")
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "directive");
        assert_eq!(events[0].ref_id, "poke-1");
        assert_eq!(events[0].priority, "high");
        assert_eq!(events[0].data.prompt, "check in");
        assert_eq!(events[0].data.source, "poke");
        assert_eq!(events[0].data.schedule_id, None);
        assert_eq!(count_undelivered(&db, "alice").await.unwrap(), 0);

        // Second call: nothing left to collect -- delivered exactly once.
        let second = collect_undelivered(&db, "alice", "2026-01-01T00:00:06Z")
            .await
            .unwrap();
        assert_eq!(second, Vec::new());
    }

    #[tokio::test]
    async fn collect_undelivered_orders_by_created_at_ascending() {
        let (_dir, db) = test_conn().await;
        create_poke(
            &db,
            "poke-2",
            "alice",
            "second",
            None,
            None,
            "2026-01-01T00:00:02Z",
        )
        .await
        .unwrap();
        create_poke(
            &db,
            "poke-1",
            "alice",
            "first",
            None,
            None,
            "2026-01-01T00:00:01Z",
        )
        .await
        .unwrap();

        let events = collect_undelivered(&db, "alice", "2026-01-01T00:00:05Z")
            .await
            .unwrap();
        let ids: Vec<&str> = events.iter().map(|e| e.ref_id.as_str()).collect();
        assert_eq!(ids, vec!["poke-1", "poke-2"]);
    }

    #[tokio::test]
    async fn collect_undelivered_is_scoped_per_agent() {
        let (_dir, db) = test_conn().await;
        create_poke(
            &db,
            "poke-1",
            "alice",
            "for alice",
            None,
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        create_poke(
            &db,
            "poke-2",
            "bob",
            "for bob",
            None,
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let events = collect_undelivered(&db, "alice", "2026-01-01T00:00:05Z")
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ref_id, "poke-1");
        // bob's poke must still be sitting undelivered.
        assert_eq!(count_undelivered(&db, "bob").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn collect_undelivered_empty_is_not_an_error() {
        let (_dir, db) = test_conn().await;
        assert_eq!(
            collect_undelivered(&db, "alice", "2026-01-01T00:00:00Z")
                .await
                .unwrap(),
            Vec::new()
        );
    }

    /// F17 class-sweep: `collect_undelivered` has the IDENTICAL shape as
    /// `scheduled_directive_repository::collect_due_and_fire` -- select
    /// into memory, per-row mark-delivered, unconditional
    /// `events.push`. Unlike the scheduled sibling (whose per-row write
    /// is a single-row `Entity::update()`, which sea-orm itself refuses
    /// with `DbErr::RecordNotUpdated` when 0 rows match), this one uses
    /// `Entity::update_many().filter(...)`, which does NOT error on 0
    /// affected rows -- pre-fix, it would silently no-op and the code
    /// would push the event regardless, an even worse failure mode
    /// than the scheduled sibling's (no error signal at all). No
    /// delete/cancel-poke endpoint exists in this codebase yet (nothing
    /// currently calls `Entity::delete_*` against this table), so this
    /// test drives the race directly against the repository function
    /// with a raw `Entity::delete_by_id`, proving the fix holds even
    /// before a real caller exists.
    ///
    /// Same deterministic [`race_hook`]-based design as the scheduled-
    /// directive race test in the sibling module (see its doc comment
    /// for the full reasoning on why "no event ever" is the wrong
    /// invariant once the fix is a transaction: "fire, then delete" is
    /// a legitimate serialization, and what the fix actually
    /// guarantees is that the concurrent delete is structurally BLOCKED
    /// by the sweep's own transaction for as long as it holds the
    /// candidate row, never interleaved mid-row).
    #[tokio::test]
    async fn concurrent_delete_is_blocked_by_the_sweeps_own_transaction_not_interleaved() {
        let (_dir, db) = test_conn().await;
        create_poke(
            &db,
            "target",
            "alice",
            "check in",
            None,
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();

        let (reached, proceed) = race_hook::arm("target");

        let db_collector = db.clone();
        let collector = tokio::spawn(async move {
            collect_undelivered(&db_collector, "alice", "2026-01-01T00:00:05Z").await
        });

        // The collector has SELECTed "target" into memory and is now
        // parked immediately before its own per-row mark-delivered
        // write for it.
        reached.notified().await;

        let db_deleter = db.clone();
        let mut deleter = tokio::spawn(async move {
            Entity::delete_by_id("target".to_string())
                .exec(&db_deleter)
                .await
                .map(|r| r.rows_affected > 0)
        });

        // Collector hasn't been released yet -- see the scheduled-
        // directive sibling test's doc comment for why this can only
        // resolve early if the sweep isn't actually excluding
        // concurrent writers (the pre-fix bug).
        let still_blocked =
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut deleter)
                .await
                .is_err();
        assert!(
            still_blocked,
            "the concurrent DELETE completed while collect_undelivered was \
             still paused mid-sweep, BEFORE this test released it -- the \
             sweep's transaction is not actually excluding concurrent \
             writers, so a delete can still interleave mid-row"
        );

        proceed.notify_one();

        let deleted = deleter.await.unwrap().unwrap();
        assert!(
            deleted,
            "delete must remove the row once the sweep releases it"
        );

        let events = collector.await.unwrap().unwrap_or_else(|e| {
            panic!(
                "collect_undelivered must not error out merely because \
                 \"target\" was concurrently deleted mid-sweep: {e}"
            )
        });

        race_hook::disarm();

        assert!(
            events.iter().any(|e| e.ref_id == "target"),
            "\"target\" legitimately existed for the sweep's entire atomic \
             transaction and must still fire -- events were {events:?}"
        );
    }

    /// F17 test-only race-injection seam, identical in shape to
    /// `scheduled_directive_repository::tests::race_hook` (see its doc
    /// comment) -- entirely `#[cfg(test)]`, guarded by `poke_id` so it
    /// is a no-op for every other test in this binary.
    pub(crate) mod race_hook {
        use std::sync::{Arc, Mutex, OnceLock};
        use tokio::sync::Notify;

        struct Hook {
            poke_id: String,
            reached: Arc<Notify>,
            proceed: Arc<Notify>,
        }

        static HOOK: OnceLock<Mutex<Option<Hook>>> = OnceLock::new();

        pub fn arm(poke_id: &str) -> (Arc<Notify>, Arc<Notify>) {
            let reached = Arc::new(Notify::new());
            let proceed = Arc::new(Notify::new());
            let slot = HOOK.get_or_init(|| Mutex::new(None));
            *slot.lock().unwrap() = Some(Hook {
                poke_id: poke_id.to_string(),
                reached: reached.clone(),
                proceed: proceed.clone(),
            });
            (reached, proceed)
        }

        pub fn disarm() {
            if let Some(slot) = HOOK.get() {
                *slot.lock().unwrap() = None;
            }
        }

        /// No-op unless armed for this exact `poke_id`.
        pub async fn pause_before_write(poke_id: &str) {
            let pair = {
                let Some(slot) = HOOK.get() else {
                    return;
                };
                let guard = slot.lock().unwrap();
                guard.as_ref().and_then(|h| {
                    (h.poke_id == poke_id).then(|| (h.reached.clone(), h.proceed.clone()))
                })
            };
            if let Some((reached, proceed)) = pair {
                reached.notify_one();
                proceed.notified().await;
            }
        }
    }
}
