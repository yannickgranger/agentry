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
use orchestrator_types::{BriefId, EventVerdict};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn no_gates() -> std::sync::Arc<orchestrator_types::lifecycle::PhaseGates> {
    use orchestrator_types::lifecycle::{GateConfig, GatePolicy, PhaseGates};
    std::sync::Arc::new(PhaseGates {
        verifying: GateConfig {
            expected_roles: vec![],
            policy: GatePolicy::AllMustPass,
        },
        reviewing: GateConfig {
            expected_roles: vec![],
            policy: GatePolicy::AllMustPass,
        },
    })
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
    projector_task(brief_id, source, projector, None, no_gates())
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
    let events = vec![
        BriefEvent::CoderStarted {
            agent_id: "agent-1".into(),
        },
        BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-test".to_owned(),
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
            role_name: "reviewer-test".to_owned(),
        },
        BriefEvent::ShipperDone {
            pr_number: 1,
            head_sha: "abc123".into(),
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
    assert!(matches!(log[0].0.state, BriefState::Authoring { .. }));
    assert!(matches!(log[1].0.state, BriefState::Verifying { .. }));
    assert!(matches!(log[2].0.state, BriefState::Reviewing { .. }));
    assert!(matches!(log[3].0.state, BriefState::Shipping { .. }));
    assert!(matches!(log[4].0.state, BriefState::Watching { .. }));
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
    let events = vec![
        BriefEvent::CoderStarted {
            agent_id: "agent-1".into(),
        },
        // Premature-shipped claim: pre-#307 a parallel emission site
        // could turn this into a Shipped verdict. The FSM walks it to
        // Verifying instead.
        BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-test".to_owned(),
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![blocker],
            role_name: "reviewer-test".to_owned(),
        },
        // Universal handler short-circuits any non-terminal state to
        // Failed{BudgetExhausted}.
        BriefEvent::BudgetExhausted,
    ];

    let log = run_to_completion(brief_id, events).await;

    assert_eq!(log.len(), 5, "one record per legal transition");
    assert!(matches!(log[0].0.state, BriefState::Authoring { .. }));
    assert!(matches!(log[1].0.state, BriefState::Verifying { .. }));
    assert!(matches!(log[2].0.state, BriefState::Reviewing { .. }));
    assert!(matches!(log[3].0.state, BriefState::Reworking { .. }));
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
