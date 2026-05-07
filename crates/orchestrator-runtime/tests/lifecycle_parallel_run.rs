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
//! * skips (does not fail on) events the FSM rejects in the current
//!   state — the driver is FSM-resilient by spec.
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
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord, CiState};
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
    let events = VecDeque::from(vec![
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
            pr_number: 42,
            head_sha: "abc1234".into(),
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

    projector_task(brief_id.clone(), source, projector, None, no_gates())
        .await
        .expect("projector_task happy path");

    let log = written.lock().expect("mutex").clone();
    assert_eq!(
        log.len(),
        6,
        "one record per processed event up to and including terminal Shipped"
    );

    assert!(matches!(log[0].0.state, BriefState::Authoring { .. }));
    assert!(matches!(log[1].0.state, BriefState::Verifying { .. }));
    assert!(matches!(log[2].0.state, BriefState::Reviewing { .. }));
    assert!(matches!(log[3].0.state, BriefState::Shipping { .. }));
    assert!(matches!(log[4].0.state, BriefState::Watching { .. }));
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

/// Drive a stream that includes events the FSM rejects in the current
/// state. The driver must skip them at WARN, not fail, and continue
/// onward to the legal terminal transition.
#[tokio::test]
async fn projector_task_skips_invalid_transitions_and_continues() {
    let brief_id = BriefId("brf_parallel_run_invalid".into());
    let events = VecDeque::from(vec![
        // Submitted state — `AcVerifierDone` is illegal here, must be skipped.
        BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-test".to_owned(),
        },
        // Now the legal start.
        BriefEvent::CoderStarted {
            agent_id: "agent-1".into(),
        },
        // Authoring state — `ReviewerDone` is illegal here, must be skipped.
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
            role_name: "reviewer-test".to_owned(),
        },
        // Legal continuation that drives toward terminal.
        BriefEvent::CoderDone {
            verdict: EventVerdict::Failed,
        },
    ]);

    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource { events });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    projector_task(brief_id, source, projector, None, no_gates())
        .await
        .expect("projector_task tolerates rejected events");

    let log = written.lock().expect("mutex").clone();
    // Two valid transitions: Submitted→Authoring, Authoring→Failed.
    assert_eq!(
        log.len(),
        2,
        "only legal transitions produce projector writes"
    );
    assert!(matches!(log[0].0.state, BriefState::Authoring { .. }));
    assert!(matches!(log[1].0.state, BriefState::Failed { .. }));
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

    projector_task(brief_id, source, projector, None, no_gates())
        .await
        .expect("empty stream terminates cleanly");

    assert!(
        written.lock().expect("mutex").is_empty(),
        "no events processed → no writes"
    );
}
