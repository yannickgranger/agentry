use orchestrator_types::lifecycle::{
    handle, BriefEvent, BriefState, BriefStateRecord, CiState, InvalidTransition, Reason,
    RetryBudget, ReworkTarget, DEFAULT_ATTEMPT_CAP, MAXIMUM_ATTEMPT_CAP,
};
use orchestrator_types::{now, BriefId, EventVerdict, ReviewFinding, Ts};

fn fresh_retry() -> RetryBudget {
    RetryBudget {
        attempt: 1,
        max: DEFAULT_ATTEMPT_CAP,
    }
}

fn coder_started() -> BriefEvent {
    BriefEvent::CoderStarted {
        agent_id: "coder-1".to_owned(),
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

    let s1 = handle(&s0, &coder_started()).expect("submitted + coder_started");
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
    )
    .expect("authoring + coder_done(shipped)");
    assert_eq!(s2, BriefState::Verifying { retry });

    let s3 = handle(
        &s2,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
        },
    )
    .expect("verifying + ac_verifier_done(shipped)");
    assert_eq!(s3, BriefState::Reviewing { retry });

    let s4 = handle(
        &s3,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
        },
    )
    .expect("reviewing + reviewer_done(shipped)");
    assert!(matches!(s4, BriefState::Shipping { .. }));

    let s5 = handle(
        &s4,
        &BriefEvent::ShipperDone {
            pr_number: 42,
            head_sha: "abc123".to_owned(),
        },
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
    )
    .expect_err("coder cannot self-rework");
    assert_eq!(err.from, s);
}

// --- verifier rework loop ---

#[test]
fn verifier_failed_pushes_to_reworking_and_increments_retry() {
    let s = BriefState::Verifying {
        retry: RetryBudget { attempt: 1, max: 3 },
    };
    let next = handle(
        &s,
        &BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Failed,
        },
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
    };
    let next = handle(
        &s,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![],
        },
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
    };
    let next = handle(
        &s,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::Rejected,
            findings: vec![],
        },
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
    let next = handle(&s, &coder_started()).expect("ok");
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
    let next = handle(&s, &BriefEvent::RebaseStarted).expect("ok");
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
    )
    .expect("ok");
    assert!(matches!(next, BriefState::Reworking { .. }));
}

// --- retry budget exhaustion ---

#[test]
fn rework_at_cap_short_circuits_to_budget_exhausted() {
    let s = BriefState::Reviewing {
        retry: RetryBudget { attempt: 3, max: 3 },
    };
    let next = handle(
        &s,
        &BriefEvent::ReviewerDone {
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![],
        },
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
        },
        BriefState::Reviewing {
            retry: fresh_retry(),
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
        let next = handle(&s, &abort()).unwrap_or_else(|_| panic!("abort denied from {s:?}"));
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
        },
        BriefState::Reviewing {
            retry: fresh_retry(),
        },
    ];
    for s in states {
        let next = handle(&s, &BriefEvent::BudgetExhausted)
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
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
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
        let err = handle(&s, &e).expect_err("Shipped is terminal");
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
        let err = handle(&s, &e).expect_err("Failed rejects non-retry events");
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
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
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
        handle(&s, &e).expect_err("submitted rejects non-starter events");
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
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
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
        handle(&s, &e).expect_err("transition should be invalid");
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
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
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
        handle(&s, &e).expect_err("transition should be invalid");
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
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
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
        handle(&s, &e).expect_err("transition should be invalid");
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
        handle(&s, &e).expect_err("transition should be invalid");
    }
    // Aborts still apply.
    let next = handle(&s, &abort()).expect("abort still works in extension");
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
        },
        BriefState::Reviewing {
            retry: fresh_retry(),
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
    let err: InvalidTransition = handle(&s, &e).expect_err("Shipped + CoderStarted is invalid");
    assert_eq!(err.from, s);
    assert_eq!(err.event, e);
    // Cloneable so the daemon can attach the pair to a log line.
    let _cloned = err.clone();
}
