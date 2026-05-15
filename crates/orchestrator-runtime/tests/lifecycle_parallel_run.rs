//! L.3a integration tests for the parallel-run FSM driver.
//!
//! Exercises `lifecycle_driver::projector_task` with the in-memory
//! [`EventSource`] / [`StateProjector`] adapters used by the L.2
//! lifecycle tests. Verifies the projector pipeline:
//!
//! * walks `orchestrator_types::lifecycle::handle` from
//!   `BriefState::Submitted` to `BriefState::Shipped` across the
//!   happy-path event stream (coder → ac-verifier → reviewer →
//!   shipper → CI),
//! * writes one record per processed event, with the synthetic cursor
//!   advancing per event,
//! * stops at the terminal state and does not consume events appended
//!   beyond it,
//! * fails the brief to `Failed{DaemonError}` when the FSM rejects an
//!   event in the current state — the driver is FSM-strict by spec
//!   (#396b-3) so silent drops are structurally impossible.
//!
//! `verdict_conn` is passed `None` here so the test exercises the
//! projector pipeline without a Redis dependency. The
//! `redis_round_trip_writes_three_keys_atomically` test in
//! `tests/lifecycle.rs` covers the live-Redis write path; the
//! parallel-run verdict emission is covered there by SETNX dedup
//! semantics already validated in the L.2 PR.
//!
//! [`EventSource`]: orchestrator_runtime::lifecycle::EventSource
//! [`StateProjector`]: orchestrator_runtime::lifecycle::StateProjector

use async_trait::async_trait;
use orchestrator_runtime::lifecycle::{
    EventSource, EventSourceError, StateProjector, StateProjectorError,
};
use orchestrator_runtime::lifecycle_driver::projector_task;
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord, CiState, Reason};
use orchestrator_types::{now, BriefId, EventVerdict};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Build a `coder → ac-verifier → reviewer → shipper → ci-watcher` chain
/// `WalkConfig` and the entry NodeId. Mirrors the production happy-path
/// shape closely enough for the parallel-run trail assertions to remain
/// meaningful after the FSM collapsed phase variants into Walking.
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
    for (n, upstream) in [
        (&acv, &coder),
        (&reviewer, &acv),
        (&shipper, &reviewer),
        (&ci, &shipper),
    ] {
        node_configs.insert(
            n.clone(),
            NodeConfig {
                class: class.clone(),
                expected_inbound: vec![upstream.clone()],
                policy: GatePolicy::AllMustPass,
            },
        );
    }

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

#[derive(Default)]
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

/// Drive the happy-path event stream through `projector_task` and
/// assert the projector observes the FSM-correct state sequence
/// terminating at `Shipped`.
#[tokio::test]
async fn projector_task_walks_happy_path_to_shipped() {
    let brief_id = BriefId("brf_parallel_run_happy".into());
    use orchestrator_types::run_data::RunData;
    use orchestrator_types::team::NodeId;
    let events = VecDeque::from(vec![
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
                pr_number: 42,
                head_sha: "abc1234".into(),
            }),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "abc1234".into(),
        },
        // Tail event AFTER terminal — must not be consumed.
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "should-not-be-read".into(),
        },
    ]);

    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource { events });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    let (walk_config, entry_node) = chain_walk();
    projector_task(
        brief_id.clone(),
        source,
        projector,
        None,
        walk_config,
        entry_node,
    )
    .await
    .expect("projector_task happy path");

    let log = written.lock().expect("mutex").clone();
    assert_eq!(
        log.len(),
        6,
        "one record per processed event up to and including terminal Shipped"
    );

    let assert_walking_at = |i: usize, name: &str| match &log[i].0.state {
        BriefState::Walking { node_id, .. } => assert_eq!(
            node_id.0, name,
            "record {i} must be Walking at {name}, got {:?}",
            node_id.0
        ),
        other => panic!("record {i} must be Walking, got {other:?}"),
    };
    assert_walking_at(0, "coder-claude-agentry");
    assert_walking_at(1, "ac-verifier-test");
    assert_walking_at(2, "reviewer-test");
    assert_walking_at(3, "shipper-agentry");
    assert_walking_at(4, "ci-watcher-agentry");
    assert!(matches!(log[5].0.state, BriefState::Shipped));

    // Cursor advances per processed event.
    let cursors: Vec<String> = log.iter().map(|(_, c)| c.clone()).collect();
    assert_eq!(
        cursors,
        vec!["step-1", "step-2", "step-3", "step-4", "step-5", "step-6"],
        "cursor counter advances monotonically"
    );

    for (record, _) in &log {
        assert_eq!(
            record.brief_id, brief_id,
            "every record carries the brief id"
        );
    }
}

/// Drive a stream whose first event is illegal in the starting state.
/// Per 396b-3, the driver no longer warns-and-skips: it fails the brief
/// to `Failed{DaemonError}`, writes a single record, and breaks the
/// loop without consuming the trailing legal events.
#[tokio::test]
async fn projector_task_fails_brief_on_invalid_transition() {
    let brief_id = BriefId("brf_parallel_run_invalid".into());
    use orchestrator_types::team::NodeId;
    let events = VecDeque::from(vec![
        // Submitted state — `RoleDone` is illegal here. The new
        // fence transitions the brief into Failed{DaemonError} and
        // breaks the loop, so the trailing events below never run.
        BriefEvent::RoleDone {
            node_id: NodeId("ac-verifier-test".into()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: None,
        },
        BriefEvent::CoderStarted {
            agent_id: "agent-1".into(),
            role_name: "coder-claude-agentry".into(),
            started_at: now(),
        },
        BriefEvent::RoleDone {
            node_id: NodeId("coder-claude-agentry".into()),
            verdict: EventVerdict::Failed,
            findings: vec![],
            run_data: None,
        },
    ]);

    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource { events });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    let (walk_config, entry_node) = chain_walk();
    projector_task(brief_id, source, projector, None, walk_config, entry_node)
        .await
        .expect("projector_task fences on invalid transition without erroring");

    let log = written.lock().expect("mutex").clone();
    assert_eq!(
        log.len(),
        1,
        "InvalidTransition fence writes exactly one record then breaks"
    );
    match &log[0].0.state {
        BriefState::Failed {
            reason: Reason::DaemonError { detail },
        } => {
            assert!(
                detail.contains("FSM rejected"),
                "DaemonError detail must mention the FSM rejection: {detail}"
            );
        }
        other => panic!("expected Failed{{DaemonError}}, got {other:?}"),
    }
}

/// Drive an empty stream. The driver must terminate cleanly when the
/// source yields `None` without ever transitioning.
#[tokio::test]
async fn projector_task_handles_empty_stream() {
    let brief_id = BriefId("brf_parallel_run_empty".into());
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource {
        events: VecDeque::new(),
    });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    let (walk_config, entry_node) = chain_walk();
    projector_task(brief_id, source, projector, None, walk_config, entry_node)
        .await
        .expect("empty stream terminates cleanly");

    assert!(
        written.lock().expect("mutex").is_empty(),
        "no events processed → no writes"
    );
}
