//! Unit tests for the wall-clock reaper (L.5 of EPIC #246).
//!
//! Three slices:
//! 1. `is_orphan` boundary table — under / at / over budget, plus
//!    terminal-state immunity.
//! 2. End-to-end `reaper::tick` against mock inventory + sink fixtures.
//! 3. The FSM match arm: pushing the canonical budget-exhaustion event
//!    through `lifecycle::handle` lands a non-terminal brief in
//!    `Failed { reason: BudgetExhausted }`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::TimeZone;
use orchestrator_runtime::lifecycle::EventSourceError;
use orchestrator_runtime::reaper::{
    self, is_orphan, BriefInventory, ReaperSink, DEFAULT_WALL_CLOCK_SECONDS,
};
use orchestrator_types::lifecycle::{
    handle, BriefEvent, BriefState, BriefStateRecord, Reason, RetryBudget, DEFAULT_ATTEMPT_CAP,
};
use orchestrator_types::{BriefId, Ts};
use tokio::sync::Mutex;

fn ts(secs: i64) -> Ts {
    chrono::Utc.timestamp_opt(secs, 0).single().expect("ts")
}

fn fresh_retry() -> RetryBudget {
    RetryBudget {
        attempt: 1,
        max: DEFAULT_ATTEMPT_CAP,
    }
}

fn no_gates() -> orchestrator_types::lifecycle::PhaseGates {
    use orchestrator_types::lifecycle::{GateConfig, GatePolicy, PhaseGates};
    PhaseGates {
        verifying: GateConfig {
            expected_roles: vec![],
            policy: GatePolicy::AllMustPass,
        },
        reviewing: GateConfig {
            expected_roles: vec![],
            policy: GatePolicy::AllMustPass,
        },
    }
}

fn record(id: &str, state: BriefState, at_secs: i64) -> BriefStateRecord {
    BriefStateRecord {
        brief_id: BriefId(id.to_string()),
        state,
        parent_brief_id: None,
        composition_role: None,
        at: ts(at_secs),
    }
}

// ---------------------------------------------------------------------------
// is_orphan boundary table
// ---------------------------------------------------------------------------

#[test]
fn is_orphan_just_under_budget_is_false() {
    // 1799s elapsed against a 1800s budget — NOT orphan.
    let r = record(
        "brf_under",
        BriefState::Authoring {
            agent_id: "c".into(),
            started_at: ts(0),
            retry: fresh_retry(),
        },
        1,
    );
    assert!(!is_orphan(&r, ts(1800), 1800));
}

#[test]
fn is_orphan_exactly_at_budget_is_false() {
    // 1800s elapsed against a 1800s budget — boundary, NOT orphan.
    // Strict greater-than avoids double-fire on a freshly-stamped
    // record whose clock matches the budget exactly.
    let r = record(
        "brf_boundary",
        BriefState::Verifying {
            retry: fresh_retry(),
            received: std::collections::BTreeMap::new(),
            expected: vec![],
            policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
        },
        0,
    );
    assert!(!is_orphan(&r, ts(1800), 1800));
}

#[test]
fn is_orphan_one_second_over_budget_is_true() {
    let r = record(
        "brf_over",
        BriefState::Reviewing {
            retry: fresh_retry(),
            received: std::collections::BTreeMap::new(),
            expected: vec![],
            policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
        },
        0,
    );
    assert!(is_orphan(&r, ts(1801), 1800));
}

#[test]
fn is_orphan_terminal_shipped_is_false_regardless_of_elapsed() {
    let r = record("brf_shipped", BriefState::Shipped, 0);
    // a year past budget — still not orphan because it's terminal.
    assert!(!is_orphan(&r, ts(1800 + 365 * 86_400), 1800));
}

#[test]
fn is_orphan_terminal_failed_is_false_regardless_of_elapsed() {
    let r = record(
        "brf_failed",
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        },
        0,
    );
    assert!(!is_orphan(&r, ts(1_000_000), 1800));
}

#[test]
fn is_orphan_clock_skew_into_future_is_false() {
    // record.at > now (host clock skew). Defensive: don't reap.
    let r = record(
        "brf_future",
        BriefState::Authoring {
            agent_id: "c".into(),
            started_at: ts(10_000),
            retry: fresh_retry(),
        },
        10_000,
    );
    assert!(!is_orphan(&r, ts(0), 1800));
}

// ---------------------------------------------------------------------------
// Mocks for the tick-loop test
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockInventory {
    records: Vec<BriefStateRecord>,
    budgets: HashMap<String, u64>,
}

#[async_trait]
impl BriefInventory for MockInventory {
    async fn list_state_records(&mut self) -> Result<Vec<BriefStateRecord>, EventSourceError> {
        Ok(self.records.clone())
    }

    async fn read_max_wall_seconds(
        &mut self,
        brief_id: &BriefId,
    ) -> Result<Option<u64>, EventSourceError> {
        Ok(self.budgets.get(&brief_id.0).copied())
    }
}

#[derive(Clone, Default)]
struct MockSink {
    pushed: Arc<Mutex<Vec<(BriefId, BriefEvent)>>>,
    killed: Arc<Mutex<Vec<BriefId>>>,
}

#[async_trait]
impl ReaperSink for MockSink {
    async fn push_event(
        &mut self,
        brief_id: &BriefId,
        event: &BriefEvent,
    ) -> Result<(), EventSourceError> {
        self.pushed
            .lock()
            .await
            .push((brief_id.clone(), event.clone()));
        Ok(())
    }

    async fn kill_containers(&mut self, brief_id: &BriefId) {
        self.killed.lock().await.push(brief_id.clone());
    }
}

// ---------------------------------------------------------------------------
// reaper::tick — mixed input set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tick_emits_one_abort_for_one_expired_non_terminal_brief() {
    // Three records:
    //   - brf_fresh: non-terminal, well under budget — NOT reaped.
    //   - brf_expired: non-terminal, far past budget — REAPED.
    //   - brf_done: terminal Shipped, ancient — NOT reaped.
    let mut inv = MockInventory {
        records: vec![
            record(
                "brf_fresh",
                BriefState::Authoring {
                    agent_id: "c".into(),
                    started_at: ts(9_900),
                    retry: fresh_retry(),
                },
                9_900,
            ),
            record(
                "brf_expired",
                BriefState::Verifying {
                    retry: fresh_retry(),
                    received: std::collections::BTreeMap::new(),
                    expected: vec![],
                    policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
                },
                0,
            ),
            record("brf_done", BriefState::Shipped, 0),
        ],
        budgets: HashMap::from([
            ("brf_fresh".into(), 1800),
            ("brf_expired".into(), 1800),
            ("brf_done".into(), 1800),
        ]),
    };
    let mut sink = MockSink::default();

    let now = ts(10_000);
    let reaped = reaper::tick(&mut inv, &mut sink, DEFAULT_WALL_CLOCK_SECONDS, now)
        .await
        .expect("tick");
    assert_eq!(reaped, 1, "exactly one brief reaped");

    let pushed = sink.pushed.lock().await.clone();
    assert_eq!(pushed.len(), 1, "exactly one event pushed");
    assert_eq!(
        pushed[0].0,
        BriefId("brf_expired".into()),
        "the expired brief was named in the push"
    );
    assert!(
        matches!(pushed[0].1, BriefEvent::BudgetExhausted),
        "canonical budget-exhaustion event"
    );

    let killed = sink.killed.lock().await.clone();
    assert_eq!(killed, vec![BriefId("brf_expired".into())]);
}

#[tokio::test]
async fn tick_uses_default_budget_when_brief_body_absent() {
    // Brief body missing from the inventory's budget map; reaper
    // falls back to the daemon-level default. Brief has been
    // Authoring for 2 hours — well past the 30 min default — so
    // it is reaped.
    let mut inv = MockInventory {
        records: vec![record(
            "brf_no_body",
            BriefState::Authoring {
                agent_id: "c".into(),
                started_at: ts(0),
                retry: fresh_retry(),
            },
            0,
        )],
        budgets: HashMap::new(),
    };
    let mut sink = MockSink::default();

    let now = ts(2 * 3600);
    let reaped = reaper::tick(&mut inv, &mut sink, DEFAULT_WALL_CLOCK_SECONDS, now)
        .await
        .expect("tick");
    assert_eq!(reaped, 1);
}

#[tokio::test]
async fn tick_with_no_records_is_a_no_op() {
    let mut inv = MockInventory::default();
    let mut sink = MockSink::default();
    let reaped = reaper::tick(
        &mut inv,
        &mut sink,
        DEFAULT_WALL_CLOCK_SECONDS,
        ts(1_000_000),
    )
    .await
    .expect("tick");
    assert_eq!(reaped, 0);
    assert!(sink.pushed.lock().await.is_empty());
    assert!(sink.killed.lock().await.is_empty());
}

// ---------------------------------------------------------------------------
// FSM mapping — the canonical match arm the reaper relies on
// ---------------------------------------------------------------------------

#[test]
fn handle_maps_budget_exhausted_to_failed_with_budget_reason_from_every_non_terminal() {
    // The reaper pushes BriefEvent::BudgetExhausted; for the wall-clock
    // expiry path to land in terminal Failed{BudgetExhausted}, every
    // non-terminal state must accept the event and yield that exact
    // terminal. This pins the contract the reaper relies on.
    let states = [
        BriefState::Submitted,
        BriefState::Authoring {
            agent_id: "c".into(),
            started_at: ts(0),
            retry: fresh_retry(),
        },
        BriefState::Verifying {
            retry: fresh_retry(),
            received: std::collections::BTreeMap::new(),
            expected: vec![],
            policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
        },
        BriefState::Reviewing {
            retry: fresh_retry(),
            received: std::collections::BTreeMap::new(),
            expected: vec![],
            policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
        },
        BriefState::Reworking {
            target: orchestrator_types::lifecycle::ReworkTarget::Coder,
            retry: fresh_retry(),
        },
        BriefState::Shipping {
            pr_number: 1,
            head_sha: "h".into(),
            retry: fresh_retry(),
        },
        BriefState::Watching {
            pr_number: 1,
            head_sha: "h".into(),
            retry: fresh_retry(),
        },
        BriefState::Extension {
            name: "ext".into(),
            data: serde_json::json!({}),
            retry: fresh_retry(),
        },
    ];
    for s in states {
        let next = handle(&s, &BriefEvent::BudgetExhausted, &no_gates())
            .unwrap_or_else(|_| panic!("BudgetExhausted denied from {s:?}"));
        assert_eq!(
            next,
            BriefState::Failed {
                reason: Reason::BudgetExhausted,
            },
            "from {s:?}"
        );
    }
}
