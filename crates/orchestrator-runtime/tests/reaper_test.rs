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
    self, is_orphan, is_trace_orphan, BriefInventory, ReaperSink, DEFAULT_WALL_CLOCK_SECONDS,
};
use orchestrator_types::lifecycle::{
    handle, BriefEvent, BriefState, BriefStateRecord, GatePolicy, NodeConfig, Reason, RetryBudget,
    WalkConfig, DEFAULT_ATTEMPT_CAP,
};
use orchestrator_types::run_data::RunData;
use orchestrator_types::team::{NodeClass, NodeId};
use orchestrator_types::{BriefId, Ts};
use tokio::sync::Mutex;

/// Build the canonical (`WalkConfig`, entry-`NodeId`) pair used by every
/// reaper FSM test: a single-node topology rooted at the canonical coder
/// role. The reaper tests only push universal-arm events (BudgetExhausted)
/// — adjacency / per-node gate config never participates, so a one-node
/// graph is sufficient.
fn no_gates() -> (WalkConfig, NodeId) {
    let entry = NodeId("coder-claude-agentry".into());
    let mut node_configs = std::collections::HashMap::new();
    node_configs.insert(
        entry.clone(),
        NodeConfig {
            class: NodeClass("container_bound".into()),
            expected_inbound: vec![],
            policy: GatePolicy::AllMustPass,
        },
    );
    (
        WalkConfig {
            adjacency: std::collections::HashMap::new(),
            node_configs,
        },
        entry,
    )
}

fn walking_coder(agent: &str) -> BriefState {
    BriefState::Walking {
        node_id: NodeId("coder-claude-agentry".into()),
        evidence: std::collections::BTreeMap::new(),
        run_data: RunData::Coder {
            agent_id: agent.into(),
        },
        retry: fresh_retry(),
    }
}

fn walking_stateless(node: &str) -> BriefState {
    BriefState::Walking {
        node_id: NodeId(node.into()),
        evidence: std::collections::BTreeMap::new(),
        run_data: RunData::None,
        retry: fresh_retry(),
    }
}

fn ts(secs: i64) -> Ts {
    chrono::Utc.timestamp_opt(secs, 0).single().expect("ts")
}

fn fresh_retry() -> RetryBudget {
    RetryBudget {
        attempt: 1,
        max: DEFAULT_ATTEMPT_CAP,
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
    let r = record("brf_under", walking_coder("c"), 1);
    assert!(!is_orphan(&r, ts(1800), 1800));
}

#[test]
fn is_orphan_exactly_at_budget_is_false() {
    // 1800s elapsed against a 1800s budget — boundary, NOT orphan.
    // Strict greater-than avoids double-fire on a freshly-stamped
    // record whose clock matches the budget exactly.
    let r = record(
        "brf_boundary",
        walking_stateless("ac-verifier-claude-agentry"),
        0,
    );
    assert!(!is_orphan(&r, ts(1800), 1800));
}

#[test]
fn is_orphan_one_second_over_budget_is_true() {
    let r = record(
        "brf_over",
        walking_stateless("reviewer-claude-agentry"),
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
    let r = record("brf_future", walking_coder("c"), 10_000);
    assert!(!is_orphan(&r, ts(0), 1800));
}

// ---------------------------------------------------------------------------
// Mocks for the tick-loop test
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockInventory {
    records: Vec<BriefStateRecord>,
    budgets: HashMap<String, u64>,
    trace_ages: HashMap<String, u64>,
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

    async fn last_trace_event_age(
        &mut self,
        brief_id: &BriefId,
        _now: Ts,
    ) -> Result<Option<u64>, EventSourceError> {
        Ok(self.trace_ages.get(&brief_id.0).copied())
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
            record("brf_fresh", walking_coder("c"), 9_900),
            record(
                "brf_expired",
                walking_stateless("ac-verifier-claude-agentry"),
                0,
            ),
            record("brf_done", BriefState::Shipped, 0),
        ],
        budgets: HashMap::from([
            ("brf_fresh".into(), 1800),
            ("brf_expired".into(), 1800),
            ("brf_done".into(), 1800),
        ]),
        trace_ages: HashMap::new(),
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
        records: vec![record("brf_no_body", walking_coder("c"), 0)],
        budgets: HashMap::new(),
        trace_ages: HashMap::new(),
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
    // Post-#495-beta-b: the only non-terminal states are `Submitted` and
    // `Walking { run_data: ... }`. The reaper's BudgetExhausted contract
    // must fire from each `RunData` variant a real brief can sit in
    // (Coder, PrTracking, OperatorDecision, Extension, None).
    let states = [
        BriefState::Submitted,
        BriefState::Walking {
            node_id: NodeId("coder-claude-agentry".into()),
            evidence: std::collections::BTreeMap::new(),
            run_data: RunData::Coder {
                agent_id: "c".into(),
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("ac-verifier-claude-agentry".into()),
            evidence: std::collections::BTreeMap::new(),
            run_data: RunData::None,
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("reviewer-claude-agentry".into()),
            evidence: std::collections::BTreeMap::new(),
            run_data: RunData::None,
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("shipper-agentry".into()),
            evidence: std::collections::BTreeMap::new(),
            run_data: RunData::PrTracking {
                pr_number: 1,
                head_sha: "h".into(),
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("ci-watcher-agentry".into()),
            evidence: std::collections::BTreeMap::new(),
            run_data: RunData::PrTracking {
                pr_number: 1,
                head_sha: "h".into(),
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("coder-claude-agentry".into()),
            evidence: std::collections::BTreeMap::new(),
            run_data: RunData::OperatorDecision {
                disagreements: vec![],
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("ext-agentry".into()),
            evidence: std::collections::BTreeMap::new(),
            run_data: RunData::Extension {
                data: serde_json::json!({}),
            },
            retry: fresh_retry(),
        },
    ];
    let (walk_config, entry_node) = no_gates();
    for s in states {
        let next = handle(&s, &BriefEvent::BudgetExhausted, &walk_config, &entry_node)
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

// ---------------------------------------------------------------------------
// is_trace_orphan — stale-trace probe boundary table
// ---------------------------------------------------------------------------

#[test]
fn is_trace_orphan_fires_when_above_threshold() {
    let r = record("brf_quiet", walking_coder("c"), 0);
    assert!(is_trace_orphan(&r, 700, 600));
}

#[test]
fn is_trace_orphan_skips_terminal() {
    let shipped = record("brf_shipped", BriefState::Shipped, 0);
    assert!(!is_trace_orphan(&shipped, 999, 600));

    let failed = record(
        "brf_failed",
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        },
        0,
    );
    assert!(!is_trace_orphan(&failed, 999, 600));
}

#[test]
fn is_trace_orphan_skips_awaiting_captain_decision() {
    let r = record(
        "brf_awaiting",
        BriefState::Walking {
            node_id: NodeId("coder-claude-agentry".into()),
            evidence: std::collections::BTreeMap::new(),
            run_data: RunData::OperatorDecision {
                disagreements: vec![],
            },
            retry: fresh_retry(),
        },
        0,
    );
    assert!(!is_trace_orphan(&r, 999, 600));
}

// ---------------------------------------------------------------------------
// reaper::tick — stale-trace path distinct from wall-clock path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tick_reaps_stale_trace_orphan_distinctly_from_wall_clock() {
    // Brief is Authoring, only 60s old (well within the 1800s wall-clock
    // budget), but its trace stream has been silent 700s — past the
    // 600s STALE_TRACE_THRESHOLD_SECONDS. The stale-trace probe must
    // fire, push BudgetExhausted, and kill containers — same effects
    // as the wall-clock path.
    let now_secs: i64 = 10_000;
    let mut inv = MockInventory {
        records: vec![record("brf_quiet", walking_coder("c"), now_secs - 60)],
        budgets: HashMap::from([("brf_quiet".into(), 1800)]),
        trace_ages: HashMap::from([("brf_quiet".into(), 700)]),
    };
    let mut sink = MockSink::default();

    let reaped = reaper::tick(
        &mut inv,
        &mut sink,
        DEFAULT_WALL_CLOCK_SECONDS,
        ts(now_secs),
    )
    .await
    .expect("tick");
    assert_eq!(reaped, 1, "one stale-trace orphan reaped");

    let pushed = sink.pushed.lock().await.clone();
    assert_eq!(pushed.len(), 1, "exactly one event pushed");
    assert_eq!(pushed[0].0, BriefId("brf_quiet".into()));
    assert!(
        matches!(pushed[0].1, BriefEvent::BudgetExhausted),
        "stale-trace path reuses BudgetExhausted"
    );

    let killed = sink.killed.lock().await.clone();
    assert_eq!(killed, vec![BriefId("brf_quiet".into())]);
}
