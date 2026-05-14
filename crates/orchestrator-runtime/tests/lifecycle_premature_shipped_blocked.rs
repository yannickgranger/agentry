//! Structural-impossibility regression test for the premature-shipped
//! bug. Pre-cutover (before #307) two non-FSM emission sites could append
//! a Shipped verdict ahead of the actual `BriefState::Shipped` terminal.
//! After #307 the FSM projector is the sole verdict-emitter; this slice
//! (#309) adds a fence for over-cap topologies and the operator
//! `RetryRequested` decode rule. The test below pins the structural
//! invariant the cutover establishes: no event sequence can reach
//! `BriefState::Shipped` without traversing the full FSM trail
//! `Authoring → Verifying → Reviewing → Shipping → Watching → Shipped`.
//!
//! In-memory adapters are minimal local equivalents of the L.2 fixtures
//! (per the brief — the L.2 test module does not re-export them across
//! integration crates). Two scenarios:
//!
//! * Happy path: full transition trail with exactly one terminal record
//!   of kind `Shipped`.
//! * Premature-shipped pattern: an intermediate `CoderDone(Shipped)`
//!   does NOT yield a terminal `Shipped` record; the FSM walks
//!   `Authoring → Verifying → Reviewing → Reworking → Failed{BudgetExhausted}`
//!   and the only terminal record is the `Failed` one.

use async_trait::async_trait;
use orchestrator_runtime::lifecycle::{
    EventSource, EventSourceError, StateProjector, StateProjectorError,
};
use orchestrator_runtime::lifecycle_driver::projector_task;
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord, CiState, Reason};
use orchestrator_types::review::{FindingOrigin, ReviewFinding, Severity};
use orchestrator_types::{now, BriefId, EventVerdict};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Build the chain topology used by the premature-shipped tests:
/// `coder → ac-verifier → reviewer → shipper → ci-watcher`, with each
/// node's `expected_inbound` set to its single upstream. Pinning the
/// chain explicitly (rather than reusing the production self-host
/// topology) keeps the test focused on the FSM trail invariant rather
/// than on the production fan-out shape.
fn chain_walk() -> (
    std::sync::Arc<orchestrator_types::lifecycle::WalkConfig>,
    std::sync::Arc<orchestrator_types::team::NodeId>,
) {
    use orchestrator_types::lifecycle::{GatePolicy, NodeConfig, WalkConfig};
    use orchestrator_types::team::{NodeClass, NodeId};
    use std::collections::HashMap;

    let coder = NodeId("coder-claude-agentry".into());
    let acv = NodeId("ac-verifier-test".into());
    let reviewer = NodeId("reviewer-test".into());
    let shipper = NodeId("shipper-agentry".into());
    let ci = NodeId("ci-watcher-agentry".into());

    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    adjacency.insert(coder.clone(), vec![acv.clone()]);
    adjacency.insert(acv.clone(), vec![reviewer.clone()]);
    adjacency.insert(reviewer.clone(), vec![shipper.clone()]);
    adjacency.insert(shipper.clone(), vec![ci.clone()]);

    let class = NodeClass("container_bound".into());
    let mut node_configs: HashMap<NodeId, NodeConfig> = HashMap::new();
    node_configs.insert(
        coder.clone(),
        NodeConfig {
            class: class.clone(),
            expected_inbound: vec![],
            policy: GatePolicy::AllMustPass,
        },
    );
    node_configs.insert(
        acv.clone(),
        NodeConfig {
            class: class.clone(),
            expected_inbound: vec![coder.clone()],
            policy: GatePolicy::AllMustPass,
        },
    );
    node_configs.insert(
        reviewer.clone(),
        NodeConfig {
            class: class.clone(),
            expected_inbound: vec![acv.clone()],
            policy: GatePolicy::AllMustPass,
        },
    );
    node_configs.insert(
        shipper.clone(),
        NodeConfig {
            class: class.clone(),
            expected_inbound: vec![reviewer.clone()],
            policy: GatePolicy::AllMustPass,
        },
    );
    node_configs.insert(
        ci.clone(),
        NodeConfig {
            class,
            expected_inbound: vec![shipper.clone()],
            policy: GatePolicy::AllMustPass,
        },
    );

    (
        std::sync::Arc::new(WalkConfig {
            adjacency,
            node_configs,
        }),
        std::sync::Arc::new(coder),
    )
}

struct MemEventSource {
    events: VecDeque<BriefEvent>,
}

#[async_trait]
impl EventSource for MemEventSource {
    async fn next(&mut self) -> Result<Option<BriefEvent>, EventSourceError> {
        Ok(self.events.pop_front())
    }
}

struct MemStateProjector {
    written: Arc<Mutex<Vec<(BriefStateRecord, String)>>>,
}

#[async_trait]
impl StateProjector for MemStateProjector {
    async fn write(
        &mut self,
        record: &BriefStateRecord,
        last_trace_id: &str,
    ) -> Result<(), StateProjectorError> {
        self.written
            .lock()
            .expect("MemStateProjector mutex poisoned")
            .push((record.clone(), last_trace_id.to_string()));
        Ok(())
    }
}

async fn run_to_completion(
    brief_id: BriefId,
    events: Vec<BriefEvent>,
) -> Vec<(BriefStateRecord, String)> {
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource {
        events: VecDeque::from(events),
    });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });
    let (walk_config, entry_node) = chain_walk();
    projector_task(
        brief_id,
        source,
        projector,
        None,
        walk_config,
        entry_node,
    )
    .await
    .expect("projector_task");
    let log = written.lock().expect("mutex").clone();
    log
}

fn count_terminal(log: &[(BriefStateRecord, String)]) -> (usize, usize) {
    let mut shipped = 0usize;
    let mut failed = 0usize;
    for (record, _) in log {
        match record.state {
            BriefState::Shipped => shipped += 1,
            BriefState::Failed { .. } => failed += 1,
            _ => {}
        }
    }
    (shipped, failed)
}

/// Scenario 1: happy path. Drives the full coder→ac-verifier→reviewer
/// →shipper→CI-success sequence and asserts the projector observes the
/// canonical state trail terminating at `Shipped`, with exactly one
/// terminal record (the FSM projector being the sole verdict-emitter).
#[tokio::test]
async fn projector_emits_one_shipped_terminal_on_happy_path() {
    let brief_id = BriefId("brf_premature_shipped_happy".into());
    use orchestrator_types::run_data::RunData;
    use orchestrator_types::team::NodeId;
    let events = vec![
        BriefEvent::CoderStarted {
            agent_id: "agent-1".into(),
            role_name: "coder-claude-agentry".into(),
            started_at: now(),
        },
        BriefEvent::RoleDone {
            node_id: NodeId("coder-claude-agentry".into()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: None,
        },
        BriefEvent::RoleDone {
            node_id: NodeId("ac-verifier-test".into()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: None,
        },
        BriefEvent::RoleDone {
            node_id: NodeId("reviewer-test".into()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: None,
        },
        BriefEvent::RoleDone {
            node_id: NodeId("shipper-agentry".into()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: Some(RunData::PrTracking {
                pr_number: 1,
                head_sha: "abc123".into(),
            }),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "abc123".into(),
        },
    ];

    let log = run_to_completion(brief_id, events).await;

    assert_eq!(
        log.len(),
        6,
        "one record per legal transition through the full trail"
    );
    let expect_node = |i: usize, name: &str| match &log[i].0.state {
        BriefState::Walking { node_id, .. } => assert_eq!(
            node_id.0, name,
            "record {i} must be Walking at {name}, got {:?}",
            node_id.0
        ),
        other => panic!("record {i} must be Walking, got {other:?}"),
    };
    expect_node(0, "coder-claude-agentry");
    expect_node(1, "ac-verifier-test");
    expect_node(2, "reviewer-test");
    expect_node(3, "shipper-agentry");
    expect_node(4, "ci-watcher-agentry");
    assert!(matches!(log[5].0.state, BriefState::Shipped));

    let (shipped, failed) = count_terminal(&log);
    assert_eq!(
        shipped, 1,
        "exactly one terminal Shipped record (FSM projector is sole verdict-emitter)"
    );
    assert_eq!(failed, 0, "happy path produces no Failed record");
}

/// Scenario 2: the premature-shipped pattern is structurally impossible.
///
/// An intermediate `CoderDone(Shipped)` event MUST NOT produce a
/// terminal `Shipped` record — the FSM transitions Authoring → Verifying
/// (not Authoring → Shipped). The reviewer then signals `ReworkNeeded`,
/// the FSM enters `Reworking`, and the universal `BudgetExhausted`
/// event short-circuits to `Failed{BudgetExhausted}`. The only terminal
/// record observed is that single Failed.
#[tokio::test]
async fn premature_shipped_event_does_not_yield_shipped_terminal() {
    let brief_id = BriefId("brf_premature_shipped_blocked".into());
    let blocker = ReviewFinding {
        file: Some("src/lib.rs".into()),
        line: Some(1),
        severity: Severity::Blocker,
        origin: FindingOrigin::Model {
            reviewer_agent_id: "reviewer-1".into(),
        },
        category: "test_fixture".into(),
        message: "blocker fixture for rework path".into(),
        suggested_fix: None,
        prohibitions: vec![],
        requirements: vec![],
    };
    use orchestrator_types::team::NodeId;
    let events = vec![
        BriefEvent::CoderStarted {
            agent_id: "agent-1".into(),
            role_name: "coder-claude-agentry".into(),
            started_at: now(),
        },
        // Premature-shipped claim: pre-#307 a parallel emission site
        // could turn this into a Shipped verdict. The FSM walks it to
        // the downstream gate instead.
        BriefEvent::RoleDone {
            node_id: NodeId("coder-claude-agentry".into()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: None,
        },
        BriefEvent::RoleDone {
            node_id: NodeId("ac-verifier-test".into()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: None,
        },
        BriefEvent::RoleDone {
            node_id: NodeId("reviewer-test".into()),
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![blocker],
            run_data: None,
        },
        // Universal handler short-circuits any non-terminal state to
        // Failed{BudgetExhausted}.
        BriefEvent::BudgetExhausted,
    ];

    let log = run_to_completion(brief_id, events).await;

    assert_eq!(log.len(), 5, "one record per legal transition");
    let assert_walking_at = |i: usize, name: &str| match &log[i].0.state {
        BriefState::Walking { node_id, .. } => assert_eq!(node_id.0, name),
        other => panic!("record {i} must be Walking at {name}, got {other:?}"),
    };
    assert_walking_at(0, "coder-claude-agentry");
    assert_walking_at(1, "ac-verifier-test");
    assert_walking_at(2, "reviewer-test");
    // ReworkNeeded restarts the walker at the entry node with the retry budget bumped.
    assert_walking_at(3, "coder-claude-agentry");
    match &log[4].0.state {
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        } => {}
        other => panic!("expected Failed{{BudgetExhausted}}, got {other:?}"),
    }

    let (shipped, failed) = count_terminal(&log);
    assert_eq!(
        shipped, 0,
        "premature-shipped pattern must NOT produce a Shipped terminal"
    );
    assert_eq!(failed, 1, "exactly one Failed terminal record");
}
