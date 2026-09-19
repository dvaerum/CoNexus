//! Delivery-transport fallback policy — the pure decision brain
//! (ADR-0021). Port of `conexus/features/delivery_policy.py`.
//!
//! Given a worker's signals (unread messages, open tasks, unassigned
//! tasks, transport-status), the per-project config, and per-worker
//! bookkeeping, [`evaluate`] decides whether to ping the worker's
//! delivery transport NOW and returns the advanced bookkeeping. It is
//! intentionally pure -- no I/O, no wall clock (`now` is passed in) --
//! so the scheduler that drives it (and its tests) fully controls
//! timing.
//!
//! Key properties (ADR-0021):
//! - A ping NEVER mutates read/done state; the *condition* is the
//!   source of truth. The agent acting clears the condition, which
//!   disarms the policy.
//! - Escalating backoff while a condition stays unmet (widen, cap),
//!   reset on clear.
//! - Status gating: never deliver to a `dead` transport; suppress while
//!   `working`; a `dormant` session is pinged only when `wake_dormant`.
//! - `cooldown_seconds` is the floor under the backoff gap.

/// The statuses a runtime may report (ADR-0021), mirrored from
/// [`crate::delivery_transport::VALID_STATUSES`]. A real enum here
/// (not the raw `String` `delivery_transport::get_status` returns) so
/// the pure policy match below is exhaustive and can't typo a status
/// string -- callers convert via [`TransportStatus::from_report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportStatus {
    Working,
    Idle,
    Dormant,
    Dead,
}

impl TransportStatus {
    /// A connected worker that hasn't reported status yet (`None`), or
    /// reported something this hub doesn't recognize, is treated as
    /// idle (deliverable) -- matches Python's `get_status(agent_id) or
    /// "idle"`, which falls through every special-cased branch
    /// (`dead`/`working`/`dormant`) for any other string exactly the
    /// same way idle does.
    pub fn from_report(status: Option<&str>) -> Self {
        match status {
            Some("working") => Self::Working,
            Some("dormant") => Self::Dormant,
            Some("dead") => Self::Dead,
            _ => Self::Idle,
        }
    }
}

/// The per-project fallback policy (mirrors the `config_delivery_*`
/// settings in `conexus-core`'s `settings_schema.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryPolicyConfig {
    pub enabled: bool,
    pub on_unread_messages: bool,
    pub on_unfinished_tasks: bool,
    pub on_unassigned_tasks: bool,
    pub on_due_directives: bool,
    pub backoff_initial_seconds: i64,
    pub backoff_max_seconds: i64,
    pub cooldown_seconds: i64,
    pub wake_dormant: bool,
}

/// A snapshot of the state the policy reasons over for one worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSignals {
    pub unread_messages: i64,
    pub open_tasks: i64,
    pub unassigned_tasks: i64,
    pub transport_status: TransportStatus,
}

/// Per-worker ping state, carried across evaluations (persisted by the
/// scheduler). Default = disarmed / never pinged this arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PingBookkeeping {
    pub armed_since: Option<i64>,
    pub last_ping_at: Option<i64>,
    pub ping_count: u32,
}

/// The highest-priority armed condition's name, or `None` if none is
/// armed -- also doubles as the frame's `reason` field verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    UnreadMessages,
    UnfinishedTasks,
    UnassignedTasks,
}

impl Reason {
    /// The exact snake_case string Python's frame `reason` field and
    /// ADR-0021's reason vocabulary use.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::UnreadMessages => "unread_messages",
            Reason::UnfinishedTasks => "unfinished_tasks",
            Reason::UnassignedTasks => "unassigned_tasks",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PingDecision {
    pub should_ping: bool,
    pub reason: Option<Reason>,
    pub bookkeeping: PingBookkeeping,
    /// Earliest `now` at which a ping could next fire (`None` when
    /// disarmed or the transport is dead) -- a hint for the
    /// scheduler's next wake.
    pub next_eligible_at: Option<i64>,
}

/// The highest-priority armed condition, or `None` if none. Messages
/// beat unfinished tasks beat unassigned tasks.
fn active_reason(config: &DeliveryPolicyConfig, signals: &WorkerSignals) -> Option<Reason> {
    if config.on_unread_messages && signals.unread_messages > 0 {
        return Some(Reason::UnreadMessages);
    }
    if config.on_unfinished_tasks && signals.open_tasks > 0 {
        return Some(Reason::UnfinishedTasks);
    }
    if config.on_unassigned_tasks && signals.unassigned_tasks > 0 {
        return Some(Reason::UnassignedTasks);
    }
    None
}

/// The minimum gap that must elapse AFTER the `ping_count`-th ping
/// before another may fire: initial × 2^(n-1), capped at max, floored
/// at cooldown.
///
/// Computed in `f64`, not integer exponentiation, deliberately: unlike
/// Python's arbitrary-precision `int`, a `ping_count` that grows
/// unboundedly under a persistently-unmet condition (this policy never
/// resets `ping_count` except on disarm) would overflow a fixed-width
/// integer shift/power long before it matters -- every config's own
/// `backoff_max_seconds` ceiling is reached within a couple dozen
/// pings at most, so capping the exponent and doing the math in `f64`
/// is exact for every value that changes the result and simply
/// saturates (harmlessly, no panic) for every value that doesn't.
fn required_gap(config: &DeliveryPolicyConfig, ping_count: u32) -> f64 {
    let exp = ping_count.saturating_sub(1).min(62);
    let backoff = (config.backoff_initial_seconds as f64) * 2f64.powi(exp as i32);
    let backoff = backoff.min(config.backoff_max_seconds as f64);
    backoff.max(config.cooldown_seconds as f64)
}

/// Decide whether to ping `now` and return the advanced bookkeeping.
pub fn evaluate(
    config: &DeliveryPolicyConfig,
    signals: &WorkerSignals,
    bookkeeping: PingBookkeeping,
    now: i64,
) -> PingDecision {
    if !config.enabled {
        return PingDecision {
            should_ping: false,
            reason: None,
            bookkeeping: PingBookkeeping::default(),
            next_eligible_at: None,
        };
    }

    let Some(reason) = active_reason(config, signals) else {
        // No condition holds -> disarm (resets backoff for the next arm).
        return PingDecision {
            should_ping: false,
            reason: None,
            bookkeeping: PingBookkeeping::default(),
            next_eligible_at: None,
        };
    };

    // Armed. Establish armed_since on first arm; keep it thereafter.
    let armed_since = bookkeeping.armed_since.unwrap_or(now);
    let bk = PingBookkeeping {
        armed_since: Some(armed_since),
        ..bookkeeping
    };

    // Status gating -- armed, but delivery may be impossible or suppressed.
    match signals.transport_status {
        TransportStatus::Dead => {
            return PingDecision {
                should_ping: false,
                reason: Some(reason),
                bookkeeping: bk,
                next_eligible_at: None,
            };
        }
        TransportStatus::Working => {
            // Suppressed now; stays armed so it fires once the session is idle.
            return PingDecision {
                should_ping: false,
                reason: Some(reason),
                bookkeeping: bk,
                next_eligible_at: None,
            };
        }
        TransportStatus::Dormant if !config.wake_dormant => {
            return PingDecision {
                should_ping: false,
                reason: Some(reason),
                bookkeeping: bk,
                next_eligible_at: None,
            };
        }
        _ => {}
    }

    // Deliverable (idle, or dormant with wake_dormant).
    let Some(last_ping_at) = bk.last_ping_at else {
        // First ping of this arm fires immediately.
        let fired = PingBookkeeping {
            last_ping_at: Some(now),
            ping_count: 1,
            ..bk
        };
        let next = now + required_gap(config, 1).round() as i64;
        return PingDecision {
            should_ping: true,
            reason: Some(reason),
            bookkeeping: fired,
            next_eligible_at: Some(next),
        };
    };

    let gap = required_gap(config, bk.ping_count).round() as i64;
    if now - last_ping_at >= gap {
        let fired = PingBookkeeping {
            last_ping_at: Some(now),
            ping_count: bk.ping_count + 1,
            ..bk
        };
        let next = now + required_gap(config, fired.ping_count).round() as i64;
        return PingDecision {
            should_ping: true,
            reason: Some(reason),
            bookkeeping: fired,
            next_eligible_at: Some(next),
        };
    }

    // Armed + deliverable but still inside the backoff window.
    PingDecision {
        should_ping: false,
        reason: Some(reason),
        bookkeeping: bk,
        next_eligible_at: Some(last_ping_at + gap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn sig() -> WorkerSignals {
        WorkerSignals {
            unread_messages: 0,
            open_tasks: 0,
            unassigned_tasks: 0,
            transport_status: TransportStatus::Idle,
        }
    }

    const FRESH: PingBookkeeping = PingBookkeeping {
        armed_since: None,
        last_ping_at: None,
        ping_count: 0,
    };

    // ── master switch + triggers ────────────────────────────────────

    #[test]
    fn disabled_never_pings() {
        let d = evaluate(
            &DeliveryPolicyConfig {
                enabled: false,
                ..cfg()
            },
            &WorkerSignals {
                unread_messages: 5,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(!d.should_ping);
    }

    #[test]
    fn no_condition_no_ping_and_disarmed() {
        let d = evaluate(&cfg(), &sig(), FRESH, 100);
        assert!(!d.should_ping);
        assert_eq!(d.bookkeeping.armed_since, None);
    }

    #[test]
    fn unread_arms_and_pings_immediately_when_idle() {
        let d = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 3,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(d.should_ping);
        assert_eq!(d.reason, Some(Reason::UnreadMessages));
        assert_eq!(d.bookkeeping.ping_count, 1);
        assert_eq!(d.bookkeeping.last_ping_at, Some(100));
    }

    #[test]
    fn trigger_toggle_off_does_not_arm() {
        let d = evaluate(
            &DeliveryPolicyConfig {
                on_unread_messages: false,
                ..cfg()
            },
            &WorkerSignals {
                unread_messages: 3,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(!d.should_ping);
    }

    #[test]
    fn unfinished_tasks_trigger() {
        let d = evaluate(
            &cfg(),
            &WorkerSignals {
                open_tasks: 2,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(d.should_ping);
        assert_eq!(d.reason, Some(Reason::UnfinishedTasks));
    }

    #[test]
    fn unassigned_off_by_default() {
        let d = evaluate(
            &cfg(),
            &WorkerSignals {
                unassigned_tasks: 4,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(!d.should_ping);

        let d2 = evaluate(
            &DeliveryPolicyConfig {
                on_unassigned_tasks: true,
                ..cfg()
            },
            &WorkerSignals {
                unassigned_tasks: 4,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(d2.should_ping);
        assert_eq!(d2.reason, Some(Reason::UnassignedTasks));
    }

    #[test]
    fn priority_order_messages_beat_unfinished_beat_unassigned() {
        let d = evaluate(
            &DeliveryPolicyConfig {
                on_unassigned_tasks: true,
                ..cfg()
            },
            &WorkerSignals {
                unread_messages: 1,
                open_tasks: 1,
                unassigned_tasks: 1,
                ..sig()
            },
            FRESH,
            100,
        );
        assert_eq!(d.reason, Some(Reason::UnreadMessages));

        let d2 = evaluate(
            &DeliveryPolicyConfig {
                on_unassigned_tasks: true,
                ..cfg()
            },
            &WorkerSignals {
                open_tasks: 1,
                unassigned_tasks: 1,
                ..sig()
            },
            FRESH,
            100,
        );
        assert_eq!(d2.reason, Some(Reason::UnfinishedTasks));
    }

    // ── escalating backoff + cooldown ───────────────────────────────

    #[test]
    fn within_backoff_does_not_re_ping() {
        let after1 = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 1,
                ..sig()
            },
            FRESH,
            100,
        )
        .bookkeeping;
        // 10s later -- well under the 60s cooldown floor.
        let d = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 1,
                ..sig()
            },
            after1,
            110,
        );
        assert!(!d.should_ping);
    }

    #[test]
    fn re_pings_after_backoff_elapses() {
        let after1 = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 1,
                ..sig()
            },
            FRESH,
            100,
        )
        .bookkeeping;
        // 60s later -- cooldown/first-backoff elapsed.
        let d = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 1,
                ..sig()
            },
            after1,
            160,
        );
        assert!(d.should_ping);
        assert_eq!(d.bookkeeping.ping_count, 2);
    }

    #[test]
    fn backoff_escalates_and_caps() {
        let cfg = DeliveryPolicyConfig {
            backoff_initial_seconds: 30,
            backoff_max_seconds: 120,
            cooldown_seconds: 0,
            ..cfg()
        };
        let mut bk = FRESH;
        let mut now: i64 = 0;
        let mut gaps: Vec<i64> = Vec::new();
        let mut prev_ping: Option<i64> = None;
        // Drive 6 pings, always eligible, record the gap the policy required.
        for _ in 0..6 {
            let mut step: i64 = 0;
            loop {
                let d = evaluate(
                    &cfg,
                    &WorkerSignals {
                        unread_messages: 1,
                        ..sig()
                    },
                    bk,
                    now + step,
                );
                if d.should_ping {
                    if let Some(prev) = prev_ping {
                        gaps.push((now + step) - prev);
                    }
                    prev_ping = Some(now + step);
                    bk = d.bookkeeping;
                    now += step;
                    break;
                }
                step += 1;
            }
        }
        // Escalating: 30, 60, 120, then capped at 120, 120…
        assert_eq!(gaps, vec![30, 60, 120, 120, 120]);
    }

    #[test]
    fn cooldown_is_the_floor() {
        let cfg = DeliveryPolicyConfig {
            backoff_initial_seconds: 5,
            cooldown_seconds: 100,
            ..cfg()
        };
        let after1 = evaluate(
            &cfg,
            &WorkerSignals {
                unread_messages: 1,
                ..sig()
            },
            FRESH,
            0,
        )
        .bookkeeping;
        // 5s (initial backoff) elapsed but under the 100s cooldown → no ping.
        assert!(
            !evaluate(
                &cfg,
                &WorkerSignals {
                    unread_messages: 1,
                    ..sig()
                },
                after1,
                5,
            )
            .should_ping
        );
        assert!(
            evaluate(
                &cfg,
                &WorkerSignals {
                    unread_messages: 1,
                    ..sig()
                },
                after1,
                100,
            )
            .should_ping
        );
    }

    #[test]
    fn condition_clear_resets_backoff() {
        let after1 = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 1,
                ..sig()
            },
            FRESH,
            100,
        )
        .bookkeeping;
        // Agent reads the message -> condition clears -> disarm.
        let cleared = evaluate(&cfg(), &sig(), after1, 110);
        assert!(!cleared.should_ping);
        assert_eq!(cleared.bookkeeping.armed_since, None);
        assert_eq!(cleared.bookkeeping.ping_count, 0);
        // A new message arms fresh -> immediate ping again (backoff reset).
        let d = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 1,
                ..sig()
            },
            cleared.bookkeeping,
            120,
        );
        assert!(d.should_ping);
        assert_eq!(d.bookkeeping.ping_count, 1);
    }

    // ── transport-status gating ──────────────────────────────────────

    #[test]
    fn working_suppresses_ping() {
        let d = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 1,
                transport_status: TransportStatus::Working,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(!d.should_ping);
        // Still armed (so it fires once idle), just suppressed now.
        assert_eq!(d.bookkeeping.armed_since, Some(100));
    }

    #[test]
    fn dead_never_pings() {
        let d = evaluate(
            &cfg(),
            &WorkerSignals {
                unread_messages: 1,
                transport_status: TransportStatus::Dead,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(!d.should_ping);
    }

    #[test]
    fn dormant_gated_by_wake_flag() {
        let off = evaluate(
            &DeliveryPolicyConfig {
                wake_dormant: false,
                ..cfg()
            },
            &WorkerSignals {
                unread_messages: 1,
                transport_status: TransportStatus::Dormant,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(!off.should_ping);

        let on = evaluate(
            &DeliveryPolicyConfig {
                wake_dormant: true,
                ..cfg()
            },
            &WorkerSignals {
                unread_messages: 1,
                transport_status: TransportStatus::Dormant,
                ..sig()
            },
            FRESH,
            100,
        );
        assert!(on.should_ping);
    }

    #[test]
    fn transport_status_from_report_defaults_unrecognized_and_missing_to_idle() {
        assert_eq!(TransportStatus::from_report(None), TransportStatus::Idle);
        assert_eq!(
            TransportStatus::from_report(Some("idle")),
            TransportStatus::Idle
        );
        assert_eq!(
            TransportStatus::from_report(Some("bogus")),
            TransportStatus::Idle
        );
        assert_eq!(
            TransportStatus::from_report(Some("working")),
            TransportStatus::Working
        );
        assert_eq!(
            TransportStatus::from_report(Some("dormant")),
            TransportStatus::Dormant
        );
        assert_eq!(
            TransportStatus::from_report(Some("dead")),
            TransportStatus::Dead
        );
    }
}
