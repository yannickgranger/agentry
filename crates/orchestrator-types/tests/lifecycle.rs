use orchestrator_types::lifecycle::{
    handle, BriefEvent, BriefState, BriefStateRecord, CiState, InvalidTransition, Reason,
    RetryBudget, ReworkTarget, DEFAULT_ATTEMPT_CAP, MAXIMUM_ATTEMPT_CAP,
};
use orchestrator_types::{now, BriefId, EventVerdict, ReviewFinding, Ts};

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

fn single_role_gates() -> orchestrator_types::lifecycle::PhaseGates {
    use orchestrator_types::lifecycle::{GateConfig, GatePolicy, PhaseGates};
    PhaseGates {
        verifying: GateConfig {
            expected_roles: vec!["ac-verifier-test".to_owned()],
            policy: GatePolicy::AllMustPass,
        },
        reviewing: GateConfig {
            expected_roles: vec!["reviewer-test".to_owned()],
            policy: GatePolicy::AllMustPass,
        },
    }
}

fn fresh_retry() -> RetryBudget {
    RetryBudget {
        attempt: 1,
        max: DEFAULT_ATTEMPT_CAP,
    }
}

fn coder_started() -> BriefEvent {
    BriefEvent::CoderStarted {
        agent_id: "coder-1".to_owned(),
        started_at: now(),
    }
}

fn abort() -> BriefEvent {
    BriefEvent::AbortRequested {
        actor: "human".to_owned(),
        message: "stop".to_owned(),
    }
}

// --- happy path ---

#[test]
fn happy_path_submitted_to_shipped() {
    let s0 = BriefState::Submitted;
    let gates = single_role_gates();

    let s1 = handle(&s0, &coder_started(), &gates).expect("submitted + coder_started");
    let retry = match &s1 {
        BriefState::Authoring {
            agent_id, retry, ..
        } => {
            assert_eq!(agent_id, "coder-1");
            assert_eq!(retry.attempt, 1);
            assert_eq!(retry.max, DEFAULT_ATTEMPT_CAP);
            *retry
        }
        other => panic!("expected Authoring, got {other:?}"),
    };

    let s2 = handle(
        &s1,
        &BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        &gates,
    )
    .expect("authoring + coder_done(shipped)");
    assert_eq!(
        s2,
        BriefState::Verifying {
            retry,
            received: std::collections::BTreeMap::new(),
            expected: vec!["ac-verifier-test".to_owned()],
            policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass
        }
    );

    let s3 = handle(
        &s2,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-test".to_owned(),
        },
        &gates,
    )
    .expect("verifying + ac_verifier_done(shipped)");
    assert_eq!(
        s3,
        BriefState::Reviewing {
            retry,
            received: std::collections::BTreeMap::new(),
            expected: vec!["reviewer-test".to_owned()],
            policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass
        }
    );

    let s4 = handle(
        &s3,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
            role_name: "reviewer-test".to_owned(),
        },
        &gates,
    )
    .expect("reviewing + reviewer_done(shipped)");
    assert!(matches!(s4, BriefState::Shipping { .. }));

    let s5 = handle(
        &s4,
        &BriefEvent::ShipperDone {
            pr_number: 42,
            head_sha: "abc123".to_owned(),
        },
        &gates,
    )
    .expect("shipping + shipper_done");
    assert_eq!(
        s5,
        BriefState::Watching {
            pr_number: 42,
            head_sha: "abc123".to_owned(),
            retry,
        }
    );

    let s6 = handle(
        &s5,
        &BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "abc123".to_owned(),
        },
        &gates,
    )
    .expect("watching + ci_success");
    assert_eq!(s6, BriefState::Shipped);
}

// --- coder failure path ---

#[test]
fn authoring_coder_done_failed_goes_to_failed_acceptance() {
    let s = BriefState::Authoring {
        agent_id: "c".into(),
        started_at: Ts::default(),
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::CoderDone {
            verdict: EventVerdict::Failed,
        },
        &no_gates(),
    )
    .expect("ok");
    assert!(
        matches!(
            next,
            BriefState::Failed {
                reason: Reason::AcceptanceFailed { .. }
            }
        ),
        "got {next:?}"
    );
}

#[test]
fn authoring_coder_done_rework_needed_is_invalid() {
    let s = BriefState::Authoring {
        agent_id: "c".into(),
        started_at: Ts::default(),
        retry: fresh_retry(),
    };
    let err = handle(
        &s,
        &BriefEvent::CoderDone {
            verdict: EventVerdict::ReworkNeeded,
        },
        &no_gates(),
    )
    .expect_err("coder cannot self-rework");
    assert_eq!(err.from, s);
}

// --- verifier rework loop ---

#[test]
fn verifier_failed_pushes_to_reworking_and_increments_retry() {
    let s = BriefState::Verifying {
        retry: RetryBudget { attempt: 1, max: 3 },
        received: std::collections::BTreeMap::new(),
        expected: vec![],
        policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
    };
    let next = handle(
        &s,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Failed,
            role_name: "ac-verifier-test".to_owned(),
        },
        &no_gates(),
    )
    .expect("ok");
    match next {
        BriefState::Reworking { target, retry } => {
            assert_eq!(target, ReworkTarget::Coder);
            assert_eq!(retry, RetryBudget { attempt: 2, max: 3 });
        }
        other => panic!("expected Reworking, got {other:?}"),
    }
}

// --- reviewer rework loop ---

#[test]
fn reviewer_rework_increments_retry() {
    let s = BriefState::Reviewing {
        retry: RetryBudget { attempt: 1, max: 3 },
        received: std::collections::BTreeMap::new(),
        expected: vec![],
        policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
    };
    let next = handle(
        &s,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![],
            role_name: "reviewer-test".to_owned(),
        },
        &no_gates(),
    )
    .expect("ok");
    match next {
        BriefState::Reworking { retry, target } => {
            assert_eq!(retry, RetryBudget { attempt: 2, max: 3 });
            assert_eq!(target, ReworkTarget::Coder);
        }
        other => panic!("expected Reworking, got {other:?}"),
    }
}

#[test]
fn reviewer_rejected_goes_to_failed() {
    let s = BriefState::Reviewing {
        retry: fresh_retry(),
        received: std::collections::BTreeMap::new(),
        expected: vec![],
        policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
    };
    let next = handle(
        &s,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::Rejected,
            findings: vec![],
            role_name: "reviewer-test".to_owned(),
        },
        &no_gates(),
    )
    .expect("ok");
    assert!(matches!(
        next,
        BriefState::Failed {
            reason: Reason::AcceptanceFailed { .. }
        }
    ));
}

// --- reworking back to authoring preserves retry ---

#[test]
fn reworking_coder_started_returns_to_authoring_with_same_retry() {
    let retry = RetryBudget { attempt: 2, max: 3 };
    let s = BriefState::Reworking {
        target: ReworkTarget::Coder,
        retry,
    };
    let next = handle(&s, &coder_started(), &no_gates()).expect("ok");
    match next {
        BriefState::Authoring {
            agent_id, retry: r, ..
        } => {
            assert_eq!(agent_id, "coder-1");
            assert_eq!(r, retry);
        }
        other => panic!("expected Authoring, got {other:?}"),
    }
}

// --- brief 472: CoderStarted carries a real wall-clock started_at ---

/// Fix verification for issue #472: pre-fix the FSM populated
/// `Authoring.started_at` with `Ts::default()` (1970-01-01T00:00:00Z)
/// because `handle` is pure and could not call `now()`. Post-fix the
/// timestamp rides on the `BriefEvent::CoderStarted` payload and the
/// FSM copies it through, so the resulting `Authoring` state carries
/// a real wall-clock stamp inside the (`before`, `after`) window
/// captured around the transition.
#[test]
fn coder_started_carries_real_timestamp() {
    let before = now();
    let event = BriefEvent::CoderStarted {
        agent_id: "agt_test".into(),
        started_at: now(),
    };
    let next = handle(&BriefState::Submitted, &event, &no_gates()).expect("ok");
    let started_at = match next {
        BriefState::Authoring { started_at, .. } => started_at,
        other => panic!("expected Authoring, got {other:?}"),
    };
    let after = now();
    assert!(
        started_at != Ts::default(),
        "started_at must not be the epoch-zero sentinel"
    );
    assert!(
        started_at >= before,
        "started_at {started_at} must be at or after the pre-event capture {before}"
    );
    assert!(
        started_at <= after,
        "started_at {started_at} must be at or before the post-event capture {after}"
    );
}

// --- watching: rebase + ci_pending stay in Watching ---

#[test]
fn watching_rebased_updates_head_sha() {
    let s = BriefState::Watching {
        pr_number: 7,
        head_sha: "old".into(),
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::Rebased {
            new_head_sha: "new".into(),
        },
        &no_gates(),
    )
    .expect("ok");
    assert_eq!(
        next,
        BriefState::Watching {
            pr_number: 7,
            head_sha: "new".into(),
            retry: fresh_retry(),
        }
    );
}

#[test]
fn watching_rebase_started_is_a_no_op() {
    let s = BriefState::Watching {
        pr_number: 7,
        head_sha: "h".into(),
        retry: fresh_retry(),
    };
    let next = handle(&s, &BriefEvent::RebaseStarted, &no_gates()).expect("ok");
    assert_eq!(next, s);
}

#[test]
fn watching_ci_pending_stays_in_watching() {
    let s = BriefState::Watching {
        pr_number: 7,
        head_sha: "h".into(),
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::CiResult {
            state: CiState::Pending,
            head_sha: "h".into(),
        },
        &no_gates(),
    )
    .expect("ok");
    assert_eq!(next, s);
}

#[test]
fn watching_ci_failed_kicks_off_rework() {
    let s = BriefState::Watching {
        pr_number: 7,
        head_sha: "h".into(),
        retry: RetryBudget { attempt: 1, max: 2 },
    };
    let next = handle(
        &s,
        &BriefEvent::CiResult {
            state: CiState::Failed,
            head_sha: "h".into(),
        },
        &no_gates(),
    )
    .expect("ok");
    assert!(matches!(next, BriefState::Reworking { .. }));
}

// --- retry budget exhaustion ---

#[test]
fn rework_at_cap_short_circuits_to_budget_exhausted() {
    let s = BriefState::Reviewing {
        retry: RetryBudget { attempt: 3, max: 3 },
        received: std::collections::BTreeMap::new(),
        expected: vec![],
        policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
    };
    let next = handle(
        &s,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![],
            role_name: "reviewer-test".to_owned(),
        },
        &no_gates(),
    )
    .expect("ok");
    assert_eq!(
        next,
        BriefState::Failed {
            reason: Reason::BudgetExhausted
        }
    );
}

#[test]
fn ci_failed_at_cap_short_circuits_to_budget_exhausted() {
    let s = BriefState::Watching {
        pr_number: 1,
        head_sha: "h".into(),
        retry: RetryBudget { attempt: 3, max: 3 },
    };
    let next = handle(
        &s,
        &BriefEvent::CiResult {
            state: CiState::Failed,
            head_sha: "h".into(),
        },
        &no_gates(),
    )
    .expect("ok");
    assert_eq!(
        next,
        BriefState::Failed {
            reason: Reason::BudgetExhausted
        }
    );
}

// --- universal aborts on non-terminal states ---

#[test]
fn abort_from_every_non_terminal_state_yields_failed_abort() {
    let states = vec![
        BriefState::Submitted,
        BriefState::Authoring {
            agent_id: "c".into(),
            started_at: Ts::default(),
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
            target: ReworkTarget::Coder,
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
        let next =
            handle(&s, &abort(), &no_gates()).unwrap_or_else(|_| panic!("abort denied from {s:?}"));
        assert!(
            matches!(
                next,
                BriefState::Failed {
                    reason: Reason::AbortRequested { .. }
                }
            ),
            "expected Failed{{AbortRequested}} from {s:?}, got {next:?}"
        );
    }
}

#[test]
fn budget_exhausted_event_from_every_non_terminal_state_yields_failed_budget() {
    let states = vec![
        BriefState::Submitted,
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
    ];
    for s in states {
        let next = handle(&s, &BriefEvent::BudgetExhausted, &no_gates())
            .unwrap_or_else(|_| panic!("budget_exhausted denied from {s:?}"));
        assert_eq!(
            next,
            BriefState::Failed {
                reason: Reason::BudgetExhausted
            }
        );
    }
}

// --- terminal-state behavior ---

#[test]
fn shipped_rejects_every_event() {
    let s = BriefState::Shipped;
    let events = [
        coder_started(),
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
            head_sha: "h".into(),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".into(),
        },
        BriefEvent::RebaseStarted,
        BriefEvent::Rebased {
            new_head_sha: "n".into(),
        },
        BriefEvent::RetryRequested {
            actor: "h".into(),
            reason: "r".into(),
        },
        abort(),
        BriefEvent::BudgetExhausted,
    ];
    for e in events {
        let err = handle(&s, &e, &no_gates()).expect_err("Shipped is terminal");
        assert_eq!(err.from, s);
        assert_eq!(err.event, e);
    }
}

#[test]
fn failed_accepts_only_retry_requested() {
    let s = BriefState::Failed {
        reason: Reason::BudgetExhausted,
    };
    // RetryRequested resets to Submitted.
    let next = handle(
        &s,
        &BriefEvent::RetryRequested {
            actor: "h".into(),
            reason: "manual retry".into(),
        },
        &no_gates(),
    )
    .expect("retry resets failed brief");
    assert_eq!(next, BriefState::Submitted);

    // Every other event is rejected.
    let bad = [
        coder_started(),
        BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        BriefEvent::ShipperDone {
            pr_number: 1,
            head_sha: "h".into(),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".into(),
        },
        BriefEvent::RebaseStarted,
        abort(),
        BriefEvent::BudgetExhausted,
    ];
    for e in bad {
        let err = handle(&s, &e, &no_gates()).expect_err("Failed rejects non-retry events");
        assert_eq!(err.from, s);
        assert_eq!(err.event, e);
    }
}

// --- isolated invalid pairs (events on the wrong state) ---

#[test]
fn submitted_rejects_non_starter_events() {
    let s = BriefState::Submitted;
    let bad = [
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
            head_sha: "h".into(),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".into(),
        },
        BriefEvent::RebaseStarted,
        BriefEvent::Rebased {
            new_head_sha: "n".into(),
        },
        BriefEvent::RetryRequested {
            actor: "h".into(),
            reason: "r".into(),
        },
    ];
    for e in bad {
        handle(&s, &e, &no_gates()).expect_err("submitted rejects non-starter events");
    }
}

#[test]
fn authoring_rejects_unrelated_events() {
    let s = BriefState::Authoring {
        agent_id: "c".into(),
        started_at: Ts::default(),
        retry: fresh_retry(),
    };
    let bad = [
        coder_started(),
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
            head_sha: "h".into(),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".into(),
        },
        BriefEvent::RebaseStarted,
        BriefEvent::Rebased {
            new_head_sha: "n".into(),
        },
        BriefEvent::RetryRequested {
            actor: "h".into(),
            reason: "r".into(),
        },
    ];
    for e in bad {
        handle(&s, &e, &no_gates()).expect_err("transition should be invalid");
    }
}

#[test]
fn watching_rejects_unrelated_events() {
    let s = BriefState::Watching {
        pr_number: 1,
        head_sha: "h".into(),
        retry: fresh_retry(),
    };
    let bad = [
        coder_started(),
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
            pr_number: 2,
            head_sha: "h2".into(),
        },
        BriefEvent::RetryRequested {
            actor: "h".into(),
            reason: "r".into(),
        },
    ];
    for e in bad {
        handle(&s, &e, &no_gates()).expect_err("transition should be invalid");
    }
}

#[test]
fn shipping_rejects_unrelated_events() {
    let s = BriefState::Shipping {
        pr_number: 0,
        head_sha: String::new(),
        retry: fresh_retry(),
    };
    let bad = [
        coder_started(),
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
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".into(),
        },
        BriefEvent::RebaseStarted,
        BriefEvent::Rebased {
            new_head_sha: "n".into(),
        },
        BriefEvent::RetryRequested {
            actor: "h".into(),
            reason: "r".into(),
        },
    ];
    for e in bad {
        handle(&s, &e, &no_gates()).expect_err("transition should be invalid");
    }
}

#[test]
fn extension_rejects_unrelated_events() {
    // Extension is a forward-compat slot; in L.1 only universal aborts apply.
    let s = BriefState::Extension {
        name: "future".into(),
        data: serde_json::json!({"k":"v"}),
        retry: fresh_retry(),
    };
    let bad = [
        coder_started(),
        BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".into(),
        },
    ];
    for e in bad {
        handle(&s, &e, &no_gates()).expect_err("transition should be invalid");
    }
    // Aborts still apply.
    let next = handle(&s, &abort(), &no_gates()).expect("abort still works in extension");
    assert!(matches!(
        next,
        BriefState::Failed {
            reason: Reason::AbortRequested { .. }
        }
    ));
}

// --- caps are exposed so topologies can validate against them ---

#[test]
fn budget_caps_have_expected_values() {
    assert_eq!(DEFAULT_ATTEMPT_CAP, 3);
    assert_eq!(MAXIMUM_ATTEMPT_CAP, 10);
}

// --- serde round-trips ---

#[test]
fn brief_state_roundtrip_every_variant() {
    let variants = vec![
        BriefState::Submitted,
        BriefState::Authoring {
            agent_id: "c".into(),
            started_at: now(),
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
            target: ReworkTarget::Reviewer,
            retry: RetryBudget { attempt: 2, max: 4 },
        },
        BriefState::Shipping {
            pr_number: 9,
            head_sha: "deadbeef".into(),
            retry: fresh_retry(),
        },
        BriefState::Watching {
            pr_number: 9,
            head_sha: "deadbeef".into(),
            retry: fresh_retry(),
        },
        BriefState::Extension {
            name: "future".into(),
            data: serde_json::json!({"k":"v"}),
            retry: fresh_retry(),
        },
        BriefState::Shipped,
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        },
        BriefState::Failed {
            reason: Reason::AbortRequested {
                actor: "human".into(),
                message: "stop".into(),
            },
        },
        BriefState::Failed {
            reason: Reason::AcceptanceFailed {
                detail: "did not pass".into(),
            },
        },
        BriefState::Failed {
            reason: Reason::DaemonError {
                detail: "redis down".into(),
            },
        },
    ];
    for v in variants {
        let s = serde_json::to_string(&v).expect("serialize");
        let back: BriefState = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(v, back, "roundtrip mismatch for {s}");
    }
}

#[test]
fn brief_event_roundtrip_every_variant() {
    let variants = vec![
        coder_started(),
        BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        BriefEvent::AcVerifierDone {
            verdict: EventVerdict::ReworkNeeded,
            role_name: "ac-verifier-test".to_owned(),
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![ReviewFinding {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                severity: orchestrator_types::Severity::Blocker,
                origin: orchestrator_types::FindingOrigin::Mechanical {
                    tool: "clippy".into(),
                    rule: Some("unused_variables".into()),
                },
                category: "lint".into(),
                message: "unused var".into(),
                suggested_fix: None,
                prohibitions: vec![],
                requirements: vec![],
            }],
            role_name: "reviewer-test".to_owned(),
        },
        BriefEvent::ShipperDone {
            pr_number: 1,
            head_sha: "h".into(),
        },
        BriefEvent::CiResult {
            state: CiState::Pending,
            head_sha: "h".into(),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".into(),
        },
        BriefEvent::CiResult {
            state: CiState::Failed,
            head_sha: "h".into(),
        },
        BriefEvent::RebaseStarted,
        BriefEvent::Rebased {
            new_head_sha: "n".into(),
        },
        BriefEvent::RetryRequested {
            actor: "h".into(),
            reason: "r".into(),
        },
        abort(),
        BriefEvent::BudgetExhausted,
    ];
    for v in variants {
        let s = serde_json::to_string(&v).expect("serialize");
        let back: BriefEvent = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(v, back, "roundtrip mismatch for {s}");
    }
}

#[test]
fn brief_state_record_roundtrip() {
    let record = BriefStateRecord {
        brief_id: BriefId("brf_test".into()),
        state: BriefState::Verifying {
            retry: fresh_retry(),
            received: std::collections::BTreeMap::new(),
            expected: vec![],
            policy: orchestrator_types::lifecycle::GatePolicy::AllMustPass,
        },
        parent_brief_id: Some(BriefId("brf_parent".into())),
        composition_role: Some("planner-child".into()),
        at: now(),
    };
    let s = serde_json::to_string(&record).expect("serialize");
    let back: BriefStateRecord = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(record, back);
}

#[test]
fn brief_state_record_optional_fields_omitted_when_none() {
    let record = BriefStateRecord {
        brief_id: BriefId("brf_test".into()),
        state: BriefState::Submitted,
        parent_brief_id: None,
        composition_role: None,
        at: now(),
    };
    let s = serde_json::to_string(&record).expect("serialize");
    assert!(
        !s.contains("parent_brief_id"),
        "parent_brief_id should be omitted: {s}"
    );
    assert!(
        !s.contains("composition_role"),
        "composition_role should be omitted: {s}"
    );
    let back: BriefStateRecord = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(record, back);
}

#[test]
fn retry_budget_roundtrip() {
    let r = RetryBudget {
        attempt: 2,
        max: MAXIMUM_ATTEMPT_CAP,
    };
    let s = serde_json::to_string(&r).expect("serialize");
    let back: RetryBudget = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(r, back);
}

#[test]
fn invalid_transition_carries_owned_pair() {
    let s = BriefState::Shipped;
    let e = coder_started();
    let err: Box<InvalidTransition> =
        handle(&s, &e, &no_gates()).expect_err("Shipped + CoderStarted is invalid");
    assert_eq!(err.from, s);
    assert_eq!(err.event, e);
    // Cloneable so the daemon can attach the pair to a log line.
    let _cloned = err.clone();
}

// --- 396b-3: evidence-based gating ---

fn three_ac_verifier_gates() -> orchestrator_types::lifecycle::PhaseGates {
    use orchestrator_types::lifecycle::{GateConfig, GatePolicy, PhaseGates};
    PhaseGates {
        verifying: GateConfig {
            expected_roles: vec![
                "ac-verifier-claude".to_owned(),
                "ac-verifier-gemini".to_owned(),
                "ac-verifier-grok".to_owned(),
            ],
            policy: GatePolicy::AllMustPass,
        },
        reviewing: GateConfig {
            // Non-empty so the post-Verifying chained auto-skip does
            // not short-circuit straight to Shipping; this helper is
            // used by tests pinning the verifier-phase gate behavior.
            expected_roles: vec!["reviewer-test".to_owned()],
            policy: GatePolicy::AllMustPass,
        },
    }
}

fn two_reviewer_gates() -> orchestrator_types::lifecycle::PhaseGates {
    use orchestrator_types::lifecycle::{GateConfig, GatePolicy, PhaseGates};
    PhaseGates {
        verifying: GateConfig {
            expected_roles: vec![],
            policy: GatePolicy::AllMustPass,
        },
        reviewing: GateConfig {
            expected_roles: vec![
                "reviewer-mechanical".to_owned(),
                "reviewer-claude".to_owned(),
            ],
            policy: GatePolicy::AllMustPass,
        },
    }
}

#[test]
fn verifying_waits_for_all_three_ac_verifiers_under_all_must_pass() {
    use orchestrator_types::lifecycle::GatePolicy;
    let gates = three_ac_verifier_gates();
    let expected = vec![
        "ac-verifier-claude".to_owned(),
        "ac-verifier-gemini".to_owned(),
        "ac-verifier-grok".to_owned(),
    ];
    let s0 = BriefState::Verifying {
        retry: fresh_retry(),
        received: std::collections::BTreeMap::new(),
        expected: expected.clone(),
        policy: GatePolicy::AllMustPass,
    };

    let s1 = handle(
        &s0,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-claude".to_owned(),
        },
        &gates,
    )
    .expect("first ac-verifier shipped");
    match &s1 {
        BriefState::Verifying {
            received,
            expected: e,
            policy,
            ..
        } => {
            assert_eq!(received.len(), 1);
            assert_eq!(
                received.get("ac-verifier-claude"),
                Some(&EventVerdict::Shipped)
            );
            assert_eq!(e, &expected);
            assert_eq!(policy, &GatePolicy::AllMustPass);
        }
        other => panic!("expected Verifying after first report, got {other:?}"),
    }

    let s2 = handle(
        &s1,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-gemini".to_owned(),
        },
        &gates,
    )
    .expect("second ac-verifier shipped");
    match &s2 {
        BriefState::Verifying { received, .. } => {
            assert_eq!(received.len(), 2);
            assert_eq!(
                received.get("ac-verifier-claude"),
                Some(&EventVerdict::Shipped)
            );
            assert_eq!(
                received.get("ac-verifier-gemini"),
                Some(&EventVerdict::Shipped)
            );
        }
        other => panic!("expected Verifying after two reports, got {other:?}"),
    }

    let s3 = handle(
        &s2,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-grok".to_owned(),
        },
        &gates,
    )
    .expect("third ac-verifier shipped");
    assert!(
        matches!(s3, BriefState::Reviewing { .. }),
        "expected Reviewing after third Shipped, got {s3:?}"
    );
}

#[test]
fn verifying_one_ac_verifier_failed_under_all_must_pass_transitions_to_reworking() {
    use orchestrator_types::lifecycle::GatePolicy;
    let gates = three_ac_verifier_gates();
    let s0 = BriefState::Verifying {
        retry: fresh_retry(),
        received: std::collections::BTreeMap::new(),
        expected: vec![
            "ac-verifier-claude".to_owned(),
            "ac-verifier-gemini".to_owned(),
            "ac-verifier-grok".to_owned(),
        ],
        policy: GatePolicy::AllMustPass,
    };
    let s1 = handle(
        &s0,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-claude".to_owned(),
        },
        &gates,
    )
    .expect("first shipped");
    let s2 = handle(
        &s1,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-gemini".to_owned(),
        },
        &gates,
    )
    .expect("second shipped");
    let s3 = handle(
        &s2,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Failed,
            role_name: "ac-verifier-grok".to_owned(),
        },
        &gates,
    )
    .expect("third failed");
    match s3 {
        BriefState::Reworking { target, retry } => {
            assert_eq!(target, ReworkTarget::Coder);
            assert_eq!(retry.attempt, 2);
        }
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        } => {
            // Acceptable when the retry was at cap; not the case for fresh_retry().
            panic!("budget exhausted unexpectedly with fresh retry");
        }
        other => panic!("expected Reworking, got {other:?}"),
    }
}

#[test]
fn verifying_one_ac_verifier_rejected_terminates_brief() {
    use orchestrator_types::lifecycle::GatePolicy;
    let gates = three_ac_verifier_gates();
    let s0 = BriefState::Verifying {
        retry: fresh_retry(),
        received: std::collections::BTreeMap::new(),
        expected: vec![
            "ac-verifier-claude".to_owned(),
            "ac-verifier-gemini".to_owned(),
            "ac-verifier-grok".to_owned(),
        ],
        policy: GatePolicy::AllMustPass,
    };
    let s1 = handle(
        &s0,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Rejected,
            role_name: "ac-verifier-claude".to_owned(),
        },
        &gates,
    )
    .expect("rejected verdict drives a transition");
    match s1 {
        BriefState::Failed {
            reason: Reason::AcceptanceFailed { detail },
        } => {
            assert!(
                detail.contains("ac-verifier-claude"),
                "detail should mention the rejecting role: {detail}"
            );
        }
        other => panic!("expected Failed{{AcceptanceFailed}}, got {other:?}"),
    }
}

#[test]
fn reviewing_waits_for_both_reviewers_under_all_must_pass() {
    use orchestrator_types::lifecycle::GatePolicy;
    let gates = two_reviewer_gates();
    let s0 = BriefState::Reviewing {
        retry: fresh_retry(),
        received: std::collections::BTreeMap::new(),
        expected: vec![
            "reviewer-mechanical".to_owned(),
            "reviewer-claude".to_owned(),
        ],
        policy: GatePolicy::AllMustPass,
    };
    let s1 = handle(
        &s0,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
            role_name: "reviewer-mechanical".to_owned(),
        },
        &gates,
    )
    .expect("first reviewer shipped");
    match &s1 {
        BriefState::Reviewing { received, .. } => {
            assert_eq!(received.len(), 1);
            assert_eq!(
                received.get("reviewer-mechanical"),
                Some(&EventVerdict::Shipped)
            );
        }
        other => panic!("expected Reviewing after first report, got {other:?}"),
    }
    let s2 = handle(
        &s1,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
            role_name: "reviewer-claude".to_owned(),
        },
        &gates,
    )
    .expect("second reviewer shipped");
    assert!(
        matches!(s2, BriefState::Shipping { .. }),
        "expected Shipping after both reviewers Shipped, got {s2:?}"
    );
}

#[test]
fn reviewing_one_reviewer_rework_needed_transitions_to_reworking() {
    use orchestrator_types::lifecycle::GatePolicy;
    let gates = two_reviewer_gates();
    let s0 = BriefState::Reviewing {
        retry: fresh_retry(),
        received: std::collections::BTreeMap::new(),
        expected: vec![
            "reviewer-mechanical".to_owned(),
            "reviewer-claude".to_owned(),
        ],
        policy: GatePolicy::AllMustPass,
    };
    let s1 = handle(
        &s0,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![],
            role_name: "reviewer-mechanical".to_owned(),
        },
        &gates,
    )
    .expect("rework verdict short-circuits the gate");
    match s1 {
        BriefState::Reworking { target, retry } => {
            assert_eq!(target, ReworkTarget::Coder);
            assert_eq!(retry.attempt, 2);
        }
        other => panic!("expected Reworking, got {other:?}"),
    }
}

// --- E/1: empty-phase auto-skip ---

fn gates_with_empty(
    verifying_empty: bool,
    reviewing_empty: bool,
) -> orchestrator_types::lifecycle::PhaseGates {
    use orchestrator_types::lifecycle::{GateConfig, GatePolicy, PhaseGates};
    PhaseGates {
        verifying: GateConfig {
            expected_roles: if verifying_empty {
                vec![]
            } else {
                vec!["ac-verifier-claude".to_owned()]
            },
            policy: GatePolicy::AllMustPass,
        },
        reviewing: GateConfig {
            expected_roles: if reviewing_empty {
                vec![]
            } else {
                vec!["reviewer-mech".to_owned()]
            },
            policy: GatePolicy::AllMustPass,
        },
    }
}

#[test]
fn authoring_to_verifying_auto_skips_empty_verifying() {
    use orchestrator_types::lifecycle::GatePolicy;
    let gates = gates_with_empty(true, false);
    let s = BriefState::Authoring {
        agent_id: "c".into(),
        started_at: Ts::default(),
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        &gates,
    )
    .expect("ok");
    match next {
        BriefState::Reviewing {
            received,
            expected,
            policy,
            ..
        } => {
            assert!(received.is_empty());
            assert_eq!(expected, vec!["reviewer-mech".to_owned()]);
            assert_eq!(policy, GatePolicy::AllMustPass);
        }
        other => panic!("expected Reviewing (auto-skipped Verifying), got {other:?}"),
    }
}

#[test]
fn authoring_to_verifying_auto_skips_both_empty_phases_to_shipping() {
    let gates = gates_with_empty(true, true);
    let s = BriefState::Authoring {
        agent_id: "c".into(),
        started_at: Ts::default(),
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        &gates,
    )
    .expect("ok");
    match next {
        BriefState::Shipping {
            pr_number,
            head_sha,
            ..
        } => {
            assert_eq!(pr_number, 0);
            assert!(head_sha.is_empty());
        }
        other => panic!("expected Shipping (auto-skipped both phases), got {other:?}"),
    }
}

#[test]
fn authoring_to_verifying_no_skip_when_verifying_has_expected() {
    use orchestrator_types::lifecycle::GatePolicy;
    let gates = gates_with_empty(false, true);
    let s = BriefState::Authoring {
        agent_id: "c".into(),
        started_at: Ts::default(),
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped,
        },
        &gates,
    )
    .expect("ok");
    match next {
        BriefState::Verifying {
            received,
            expected,
            policy,
            ..
        } => {
            assert!(received.is_empty());
            assert_eq!(expected, vec!["ac-verifier-claude".to_owned()]);
            assert_eq!(policy, GatePolicy::AllMustPass);
        }
        other => panic!("expected Verifying (no auto-skip), got {other:?}"),
    }
}

#[test]
fn verifying_to_reviewing_auto_skips_empty_reviewing() {
    use orchestrator_types::lifecycle::GatePolicy;
    let gates = gates_with_empty(false, true);
    let mut received = std::collections::BTreeMap::new();
    received.insert("ac-verifier-claude".to_owned(), EventVerdict::Shipped);
    let s = BriefState::Verifying {
        retry: fresh_retry(),
        received,
        expected: vec!["ac-verifier-claude".to_owned()],
        policy: GatePolicy::AllMustPass,
    };
    let next = handle(
        &s,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-claude".to_owned(),
        },
        &gates,
    )
    .expect("ok");
    match next {
        BriefState::Shipping {
            pr_number,
            head_sha,
            ..
        } => {
            assert_eq!(pr_number, 0);
            assert!(head_sha.is_empty());
        }
        other => panic!("expected Shipping (auto-skipped empty Reviewing), got {other:?}"),
    }
}
