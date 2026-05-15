//! #539 phase-7 fence — `daemon::fsm_settled` is the content-based
//! barrier predicate that lets the team-orchestration loop wait for
//! the FSM driver to consume a batch before recomputing its ready-set
//! from `Walking.evidence` (phase 7b drops the in-process
//! `shipped_roles`/`reworks_used` union once this is wired). The
//! cursor (`state_projector_cursor`) is a synthetic `step-N` counter,
//! not a trace id, so a cursor barrier is impossible — this predicate
//! is the only reliable settle signal. Pinned deterministically here
//! because a live rework canary is flaky to trigger.

use orchestrator_runtime::daemon::fsm_settled;
use orchestrator_types::lifecycle::{BriefState, Reason, RetryBudget};
use orchestrator_types::run_data::RunData;
use orchestrator_types::{EventVerdict, NodeId, RoleName, RoleRef};
use std::collections::BTreeMap;

fn role(name: &str) -> RoleRef {
    RoleRef {
        name: RoleName(name.to_string()),
        version: 1,
    }
}

fn walking(evidence: Vec<(&str, EventVerdict)>, attempt: u32) -> BriefState {
    let mut ev: BTreeMap<NodeId, EventVerdict> = BTreeMap::new();
    for (n, v) in evidence {
        ev.insert(NodeId(n.to_string()), v);
    }
    BriefState::Walking {
        node_id: NodeId("coder-claude-agentry".into()),
        evidence: ev,
        run_data: RunData::None,
        retry: RetryBudget { attempt, max: 3 },
    }
}

#[test]
fn terminal_states_are_always_settled() {
    assert!(fsm_settled(&BriefState::Shipped, 0, &[], false));
    assert!(fsm_settled(
        &BriefState::Failed {
            reason: Reason::BudgetExhausted
        },
        2,
        &[role("coder-claude-agentry")],
        true
    ));
}

#[test]
fn submitted_is_never_settled() {
    assert!(!fsm_settled(&BriefState::Submitted, 0, &[], false));
}

#[test]
fn advance_batch_settled_only_when_all_expected_shipped() {
    let expect = [
        role("coder-claude-agentry"),
        role("ac-verifier-claude-agentry"),
    ];
    // FSM evidence has only one of the two → not settled.
    let partial = walking(vec![("coder-claude-agentry", EventVerdict::Shipped)], 0);
    assert!(!fsm_settled(&partial, 0, &expect, false));

    // Both present + Shipped → settled.
    let full = walking(
        vec![
            ("coder-claude-agentry", EventVerdict::Shipped),
            ("ac-verifier-claude-agentry", EventVerdict::Shipped),
        ],
        0,
    );
    assert!(fsm_settled(&full, 0, &expect, false));
}

#[test]
fn advance_batch_not_settled_if_evidence_present_but_not_shipped() {
    let expect = [role("reviewer-mechanical-agentry")];
    // Present but ReworkNeeded (not Shipped) → not settled.
    let ev = walking(
        vec![("reviewer-mechanical-agentry", EventVerdict::ReworkNeeded)],
        0,
    );
    assert!(!fsm_settled(&ev, 0, &expect, false));
}

#[test]
fn rework_batch_settled_only_when_retry_attempt_advanced() {
    // had_rework: the FSM resets evidence to empty + bumps attempt on
    // the ReworkNeeded RoleDone. Emptied evidence alone can't be
    // distinguished from "not yet consumed" — the signal is the
    // attempt bump.
    let started_attempt = 1;

    // FSM hasn't consumed the rework yet: attempt unchanged, evidence
    // still carries the pre-rework shipped roles.
    let stale = walking(vec![("coder-claude-agentry", EventVerdict::Shipped)], 1);
    assert!(!fsm_settled(&stale, started_attempt, &[], true));

    // FSM consumed the rework: evidence reset to empty, attempt bumped.
    let reset = walking(vec![], 2);
    assert!(fsm_settled(&reset, started_attempt, &[], true));
}

#[test]
fn rework_signal_ignores_expect_shipped() {
    // On a rework batch the expect_shipped list is irrelevant — only
    // the attempt bump matters (evidence is wiped by the FSM).
    let expect = [role("coder-claude-agentry")];
    let reset = walking(vec![], 5);
    assert!(fsm_settled(&reset, 4, &expect, true));
    let not_yet = walking(vec![], 4);
    assert!(!fsm_settled(&not_yet, 4, &expect, true));
}
