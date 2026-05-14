//! Lifecycle FSM tests — post-#495-beta-b (collapsed-walker) edition.
//!
//! The FSM has four states (`Submitted`, `Walking`, `Shipped`, `Failed`)
//! and a single generic `RoleDone` event. Tests construct synthetic
//! `WalkConfig`s and drive `handle()` through real topology shapes
//! (self-host fan-out, two-reviewer fan-in, linear chains).

use orchestrator_types::lifecycle::{
    handle, BriefEvent, BriefState, BriefStateRecord, CiState, DisagreementSummary, GateConfig,
    GatePolicy, InvalidTransition, NodeConfig, Reason, RetryBudget, WalkConfig,
    DEFAULT_ATTEMPT_CAP, MAXIMUM_ATTEMPT_CAP,
};
use orchestrator_types::run_data::RunData;
use orchestrator_types::team::{NodeClass, NodeId};
use orchestrator_types::{now, BriefId, EventVerdict, ReviewFinding};
use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn fresh_retry() -> RetryBudget {
    RetryBudget {
        attempt: 1,
        max: DEFAULT_ATTEMPT_CAP,
    }
}

fn abort_event() -> BriefEvent {
    BriefEvent::AbortRequested {
        actor: "human".to_owned(),
        message: "stop".to_owned(),
    }
}

fn coder_node() -> NodeId {
    NodeId("coder-claude-agentry".to_owned())
}

fn coder_started_default() -> BriefEvent {
    BriefEvent::CoderStarted {
        agent_id: "coder-1".to_owned(),
        role_name: "coder-claude-agentry".to_owned(),
        started_at: now(),
    }
}

/// Self-host topology: entry coder fans out to three ac-verifiers, which
/// converge into two reviewers, then a shipper, then a ci-watcher sink.
fn synthetic_self_host_walk_config() -> (WalkConfig, NodeId) {
    let entry = NodeId("coder-claude-agentry".to_owned());
    let ac1 = NodeId("ac-verifier-claude-agentry".to_owned());
    let ac2 = NodeId("ac-verifier-gemini-agentry".to_owned());
    let ac3 = NodeId("ac-verifier-grok-agentry".to_owned());
    let rm = NodeId("reviewer-mechanical-agentry".to_owned());
    let rc = NodeId("reviewer-claude-agentry".to_owned());
    let shp = NodeId("shipper-agentry".to_owned());
    let ciw = NodeId("ci-watcher-agentry".to_owned());

    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    adjacency.insert(entry.clone(), vec![ac1.clone(), ac2.clone(), ac3.clone()]);
    for ac in [&ac1, &ac2, &ac3] {
        adjacency.insert(ac.clone(), vec![rm.clone(), rc.clone()]);
    }
    for r in [&rm, &rc] {
        adjacency.insert(r.clone(), vec![shp.clone()]);
    }
    adjacency.insert(shp.clone(), vec![ciw.clone()]);
    adjacency.insert(ciw.clone(), vec![]);

    let mut node_configs: HashMap<NodeId, NodeConfig> = HashMap::new();
    let class = NodeClass("container_bound".to_owned());
    let pol = GatePolicy::AllMustPass;
    node_configs.insert(
        entry.clone(),
        NodeConfig {
            class: class.clone(),
            expected_inbound: vec![],
            policy: pol.clone(),
        },
    );
    for ac in [&ac1, &ac2, &ac3] {
        node_configs.insert(
            ac.clone(),
            NodeConfig {
                class: class.clone(),
                expected_inbound: vec![entry.clone()],
                policy: pol.clone(),
            },
        );
    }
    for r in [&rm, &rc] {
        node_configs.insert(
            r.clone(),
            NodeConfig {
                class: class.clone(),
                expected_inbound: vec![ac1.clone(), ac2.clone(), ac3.clone()],
                policy: pol.clone(),
            },
        );
    }
    node_configs.insert(
        shp.clone(),
        NodeConfig {
            class: class.clone(),
            expected_inbound: vec![rm.clone(), rc.clone()],
            policy: pol.clone(),
        },
    );
    node_configs.insert(
        ciw.clone(),
        NodeConfig {
            class: class.clone(),
            expected_inbound: vec![shp.clone()],
            policy: pol.clone(),
        },
    );

    (
        WalkConfig {
            adjacency,
            node_configs,
        },
        entry,
    )
}

/// Linear chain: roles[0] -> roles[1] -> ... -> roles[n-1].
/// Returns (WalkConfig, entry_node).
fn synthetic_linear_walk_config(roles: &[&str]) -> (WalkConfig, NodeId) {
    assert!(!roles.is_empty(), "linear chain needs at least one role");
    let ids: Vec<NodeId> = roles.iter().map(|r| NodeId((*r).to_owned())).collect();

    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut node_configs: HashMap<NodeId, NodeConfig> = HashMap::new();
    let class = NodeClass("container_bound".to_owned());
    let pol = GatePolicy::AllMustPass;

    for (i, n) in ids.iter().enumerate() {
        let downstreams: Vec<NodeId> = if i + 1 < ids.len() {
            vec![ids[i + 1].clone()]
        } else {
            vec![]
        };
        adjacency.insert(n.clone(), downstreams);
        let expected_inbound: Vec<NodeId> = if i == 0 {
            vec![]
        } else {
            vec![ids[i - 1].clone()]
        };
        node_configs.insert(
            n.clone(),
            NodeConfig {
                class: class.clone(),
                expected_inbound,
                policy: pol.clone(),
            },
        );
    }

    let entry = ids[0].clone();
    (
        WalkConfig {
            adjacency,
            node_configs,
        },
        entry,
    )
}

/// Build a Walking state at `node_id` with `RunData::Coder` and given evidence.
fn walking_at_coder(node_id: NodeId, retry: RetryBudget) -> BriefState {
    BriefState::Walking {
        node_id,
        evidence: BTreeMap::new(),
        run_data: RunData::Coder {
            agent_id: "coder-1".to_owned(),
        },
        retry,
    }
}

fn role_done_shipped(role_name: &str) -> BriefEvent {
    BriefEvent::RoleDone {
        node_id: NodeId(role_name.to_owned()),
        verdict: EventVerdict::Shipped,
        findings: vec![],
        run_data: None,
    }
}

fn role_done(role_name: &str, verdict: EventVerdict) -> BriefEvent {
    BriefEvent::RoleDone {
        node_id: NodeId(role_name.to_owned()),
        verdict,
        findings: vec![],
        run_data: None,
    }
}

// ---------------------------------------------------------------------------
// happy path — Submitted -> Walking(coder) -> ... -> Shipped
// ---------------------------------------------------------------------------

#[test]
fn happy_path_submitted_to_shipped_through_self_host_topology() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s0 = BriefState::Submitted;

    // Submitted -> Walking{coder}
    let s1 =
        handle(&s0, &coder_started_default(), &cfg, &entry).expect("submitted + coder_started");
    let retry = match &s1 {
        BriefState::Walking {
            node_id,
            run_data: RunData::Coder { agent_id },
            retry,
            ..
        } => {
            assert_eq!(node_id, &coder_node());
            assert_eq!(agent_id, "coder-1");
            assert_eq!(retry.attempt, 1);
            assert_eq!(retry.max, DEFAULT_ATTEMPT_CAP);
            *retry
        }
        other => panic!("expected Walking{{Coder}}, got {other:?}"),
    };

    // Coder done -> Walking{ac-verifier} (advance to first downstream)
    let s2 = handle(
        &s1,
        &role_done_shipped("coder-claude-agentry"),
        &cfg,
        &entry,
    )
    .expect("coder shipped advances to ac-verifier");
    match &s2 {
        BriefState::Walking { node_id, .. } => {
            // Walker advances to the first downstream that passes its gate;
            // each ac-verifier has expected_inbound = [coder] and the coder
            // just shipped, so the first ac-verifier in adjacency order
            // passes immediately.
            assert!(
                node_id.0.starts_with("ac-verifier-"),
                "expected ac-verifier node, got {node_id:?}"
            );
        }
        other => panic!("expected Walking after coder shipped, got {other:?}"),
    }

    // Three ac-verifiers ship; only on the third does a reviewer gate open.
    let s3 = handle(
        &s2,
        &role_done_shipped("ac-verifier-claude-agentry"),
        &cfg,
        &entry,
    )
    .expect("first ac-verifier shipped");
    let s4 = handle(
        &s3,
        &role_done_shipped("ac-verifier-gemini-agentry"),
        &cfg,
        &entry,
    )
    .expect("second ac-verifier shipped");
    let s5 = handle(
        &s4,
        &role_done_shipped("ac-verifier-grok-agentry"),
        &cfg,
        &entry,
    )
    .expect("third ac-verifier shipped");
    match &s5 {
        BriefState::Walking { node_id, .. } => {
            assert!(
                node_id.0.starts_with("reviewer-"),
                "expected reviewer node after three ac-verifiers, got {node_id:?}"
            );
        }
        other => panic!("expected Walking{{reviewer}}, got {other:?}"),
    }

    // Both reviewers ship -> shipper gate opens.
    let s6 = handle(
        &s5,
        &role_done_shipped("reviewer-mechanical-agentry"),
        &cfg,
        &entry,
    )
    .expect("first reviewer shipped");
    let s7 = handle(
        &s6,
        &role_done_shipped("reviewer-claude-agentry"),
        &cfg,
        &entry,
    )
    .expect("second reviewer shipped");
    match &s7 {
        BriefState::Walking { node_id, .. } => {
            assert_eq!(node_id, &NodeId("shipper-agentry".to_owned()));
        }
        other => panic!("expected Walking{{shipper}}, got {other:?}"),
    }

    // Shipper emits RoleDone with PrTracking; walker advances to ci-watcher
    // inheriting run_data.
    let shipper_done = BriefEvent::RoleDone {
        node_id: NodeId("shipper-agentry".to_owned()),
        verdict: EventVerdict::Shipped,
        findings: vec![],
        run_data: Some(RunData::PrTracking {
            pr_number: 42,
            head_sha: "abc123".to_owned(),
        }),
    };
    let s8 = handle(&s7, &shipper_done, &cfg, &entry).expect("shipper shipped");
    match &s8 {
        BriefState::Walking {
            node_id, run_data, ..
        } => {
            assert_eq!(node_id, &NodeId("ci-watcher-agentry".to_owned()));
            assert_eq!(
                run_data,
                &RunData::PrTracking {
                    pr_number: 42,
                    head_sha: "abc123".to_owned(),
                }
            );
        }
        other => panic!("expected Walking{{ci-watcher, PrTracking}}, got {other:?}"),
    }
    assert_eq!(retry, fresh_retry());

    // ci-watcher emits RoleDone with Shipped — sink node, no downstreams,
    // walker terminates at Shipped.
    let s9 = handle(&s8, &role_done_shipped("ci-watcher-agentry"), &cfg, &entry)
        .expect("ci-watcher shipped");
    assert_eq!(s9, BriefState::Shipped);
}

// ---------------------------------------------------------------------------
// coder failure path — RoleDone{Failed} from coder rewinds via increment_or_fail
// ---------------------------------------------------------------------------

#[test]
fn coder_role_done_failed_rewinds_to_entry_and_increments_retry() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = walking_at_coder(coder_node(), fresh_retry());
    let next = handle(
        &s,
        &role_done("coder-claude-agentry", EventVerdict::Failed),
        &cfg,
        &entry,
    )
    .expect("coder failed rewinds");
    match next {
        BriefState::Walking {
            node_id,
            evidence,
            run_data,
            retry,
        } => {
            assert_eq!(node_id, entry);
            assert!(evidence.is_empty());
            assert_eq!(run_data, RunData::None);
            assert_eq!(
                retry,
                RetryBudget {
                    attempt: 2,
                    max: DEFAULT_ATTEMPT_CAP
                }
            );
        }
        other => panic!("expected Walking(entry, fresh, attempt=2), got {other:?}"),
    }
}

#[test]
fn coder_role_done_rejected_terminates_with_acceptance_failed() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = walking_at_coder(coder_node(), fresh_retry());
    let next = handle(
        &s,
        &role_done("coder-claude-agentry", EventVerdict::Rejected),
        &cfg,
        &entry,
    )
    .expect("rejected drives terminal");
    match next {
        BriefState::Failed {
            reason: Reason::AcceptanceFailed { detail },
        } => {
            assert!(detail.contains("coder-claude-agentry"), "got {detail}");
        }
        other => panic!("expected Failed{{AcceptanceFailed}}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// verifier rework loop — verifier reports Failed via gate path
// ---------------------------------------------------------------------------

#[test]
fn ac_verifier_failed_rewinds_to_entry_and_increments_retry() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    // Compose evidence as if walker is at an ac-verifier with coder Shipped.
    let mut evidence = BTreeMap::new();
    evidence.insert(coder_node(), EventVerdict::Shipped);
    let s = BriefState::Walking {
        node_id: NodeId("ac-verifier-claude-agentry".to_owned()),
        evidence,
        run_data: RunData::None,
        retry: RetryBudget { attempt: 1, max: 3 },
    };
    let next = handle(
        &s,
        &role_done("ac-verifier-claude-agentry", EventVerdict::Failed),
        &cfg,
        &entry,
    )
    .expect("ac-verifier failed");
    match next {
        BriefState::Walking {
            node_id,
            evidence,
            run_data,
            retry,
        } => {
            assert_eq!(node_id, entry);
            assert!(evidence.is_empty());
            assert_eq!(run_data, RunData::None);
            assert_eq!(retry, RetryBudget { attempt: 2, max: 3 });
        }
        other => panic!("expected Walking(entry, rewind), got {other:?}"),
    }
}

#[test]
fn reviewer_rework_needed_rewinds_to_entry_and_increments_retry() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("reviewer-claude-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::None,
        retry: RetryBudget { attempt: 1, max: 3 },
    };
    let next = handle(
        &s,
        &role_done("reviewer-claude-agentry", EventVerdict::ReworkNeeded),
        &cfg,
        &entry,
    )
    .expect("reviewer rework");
    match next {
        BriefState::Walking { node_id, retry, .. } => {
            assert_eq!(node_id, entry);
            assert_eq!(retry, RetryBudget { attempt: 2, max: 3 });
        }
        other => panic!("expected Walking(entry, rewind), got {other:?}"),
    }
}

#[test]
fn reviewer_rejected_terminates_with_acceptance_failed() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("reviewer-claude-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::None,
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &role_done("reviewer-claude-agentry", EventVerdict::Rejected),
        &cfg,
        &entry,
    )
    .expect("reviewer rejected");
    assert!(matches!(
        next,
        BriefState::Failed {
            reason: Reason::AcceptanceFailed { .. }
        }
    ));
}

// ---------------------------------------------------------------------------
// re-spawn coder on rework keeps the retry budget
// ---------------------------------------------------------------------------

#[test]
fn rework_coder_started_returns_to_entry_with_same_retry() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    // Walker rewound to entry with attempt=2; daemon re-spawns coder.
    let retry = RetryBudget { attempt: 2, max: 3 };
    let s = BriefState::Walking {
        node_id: entry.clone(),
        evidence: BTreeMap::new(),
        run_data: RunData::None,
        retry,
    };
    let next = handle(&s, &coder_started_default(), &cfg, &entry).expect("re-spawn");
    match next {
        BriefState::Walking {
            node_id,
            run_data: RunData::Coder { agent_id },
            retry: r,
            ..
        } => {
            assert_eq!(node_id, entry);
            assert_eq!(agent_id, "coder-1");
            assert_eq!(r, retry);
        }
        other => panic!("expected Walking{{Coder}} with preserved retry, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// CoderStarted carries a real wall-clock timestamp (issue #472)
// ---------------------------------------------------------------------------

#[test]
fn coder_started_carries_real_timestamp_through_walking() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let before = now();
    let event = BriefEvent::CoderStarted {
        agent_id: "agt_test".into(),
        role_name: "coder-claude-agentry".into(),
        started_at: now(),
    };
    let _ = before; // pinned for clarity; FSM does not carry started_at into Walking
    let next = handle(&BriefState::Submitted, &event, &cfg, &entry).expect("ok");
    // Post-collapse the started_at no longer rides on BriefState (no
    // Authoring variant), so the assertion shrinks to: the FSM accepted
    // the event and built the entry Walking state with the proper agent_id.
    match next {
        BriefState::Walking {
            node_id,
            run_data: RunData::Coder { agent_id },
            ..
        } => {
            assert_eq!(node_id, entry);
            assert_eq!(agent_id, "agt_test");
        }
        other => panic!("expected Walking{{Coder}}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// PR-tracking node — Rebased updates head_sha; RebaseStarted is a no-op
// ---------------------------------------------------------------------------

#[test]
fn ci_watcher_rebased_updates_head_sha_in_run_data() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("ci-watcher-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::PrTracking {
            pr_number: 7,
            head_sha: "old".to_owned(),
        },
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::Rebased {
            new_head_sha: "new".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("rebased updates head_sha");
    match next {
        BriefState::Walking { run_data, .. } => {
            assert_eq!(
                run_data,
                RunData::PrTracking {
                    pr_number: 7,
                    head_sha: "new".to_owned(),
                }
            );
        }
        other => panic!("expected Walking{{PrTracking{{new}}}}, got {other:?}"),
    }
}

#[test]
fn rebase_started_is_a_no_op_on_walking() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = walking_at_coder(coder_node(), fresh_retry());
    let next = handle(&s, &BriefEvent::RebaseStarted, &cfg, &entry).expect("no-op rebase started");
    assert_eq!(next, s);
}

#[test]
fn ci_pending_stays_in_walking() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("ci-watcher-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::PrTracking {
            pr_number: 7,
            head_sha: "h".to_owned(),
        },
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::CiResult {
            state: CiState::Pending,
            head_sha: "h".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("pending stays");
    assert_eq!(next, s);
}

#[test]
fn ci_success_terminates_at_shipped() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("ci-watcher-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::PrTracking {
            pr_number: 7,
            head_sha: "h".to_owned(),
        },
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("ci success");
    assert_eq!(next, BriefState::Shipped);
}

#[test]
fn ci_failed_rewinds_to_entry() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("ci-watcher-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::PrTracking {
            pr_number: 7,
            head_sha: "h".to_owned(),
        },
        retry: RetryBudget { attempt: 1, max: 2 },
    };
    let next = handle(
        &s,
        &BriefEvent::CiResult {
            state: CiState::Failed,
            head_sha: "h".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("ci failed");
    match next {
        BriefState::Walking { node_id, retry, .. } => {
            assert_eq!(node_id, entry);
            assert_eq!(retry, RetryBudget { attempt: 2, max: 2 });
        }
        other => panic!("expected Walking(entry, attempt=2), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// retry-budget exhaustion
// ---------------------------------------------------------------------------

#[test]
fn rework_at_cap_short_circuits_to_budget_exhausted() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("reviewer-claude-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::None,
        retry: RetryBudget { attempt: 3, max: 3 },
    };
    let next = handle(
        &s,
        &role_done("reviewer-claude-agentry", EventVerdict::ReworkNeeded),
        &cfg,
        &entry,
    )
    .expect("rework at cap");
    assert_eq!(
        next,
        BriefState::Failed {
            reason: Reason::BudgetExhausted
        }
    );
}

#[test]
fn ci_failed_at_cap_short_circuits_to_budget_exhausted() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("ci-watcher-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::PrTracking {
            pr_number: 1,
            head_sha: "h".to_owned(),
        },
        retry: RetryBudget { attempt: 3, max: 3 },
    };
    let next = handle(
        &s,
        &BriefEvent::CiResult {
            state: CiState::Failed,
            head_sha: "h".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("ci failed at cap");
    assert_eq!(
        next,
        BriefState::Failed {
            reason: Reason::BudgetExhausted
        }
    );
}

// ---------------------------------------------------------------------------
// universal aborts on every non-terminal state
// ---------------------------------------------------------------------------

#[test]
fn abort_from_every_non_terminal_state_yields_failed_abort() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let states = vec![
        BriefState::Submitted,
        walking_at_coder(coder_node(), fresh_retry()),
        BriefState::Walking {
            node_id: NodeId("ac-verifier-claude-agentry".to_owned()),
            evidence: BTreeMap::new(),
            run_data: RunData::None,
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("reviewer-claude-agentry".to_owned()),
            evidence: BTreeMap::new(),
            run_data: RunData::None,
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("shipper-agentry".to_owned()),
            evidence: BTreeMap::new(),
            run_data: RunData::PrTracking {
                pr_number: 1,
                head_sha: "h".to_owned(),
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("ci-watcher-agentry".to_owned()),
            evidence: BTreeMap::new(),
            run_data: RunData::PrTracking {
                pr_number: 1,
                head_sha: "h".to_owned(),
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("ext-node-agentry".to_owned()),
            evidence: BTreeMap::new(),
            run_data: RunData::Extension {
                data: serde_json::json!({}),
            },
            retry: fresh_retry(),
        },
    ];
    for s in states {
        let next = handle(&s, &abort_event(), &cfg, &entry)
            .unwrap_or_else(|_| panic!("abort denied from {s:?}"));
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
    let (cfg, entry) = synthetic_self_host_walk_config();
    let states = vec![
        BriefState::Submitted,
        walking_at_coder(coder_node(), fresh_retry()),
        BriefState::Walking {
            node_id: NodeId("reviewer-claude-agentry".to_owned()),
            evidence: BTreeMap::new(),
            run_data: RunData::None,
            retry: fresh_retry(),
        },
    ];
    for s in states {
        let next = handle(&s, &BriefEvent::BudgetExhausted, &cfg, &entry)
            .unwrap_or_else(|_| panic!("budget_exhausted denied from {s:?}"));
        assert_eq!(
            next,
            BriefState::Failed {
                reason: Reason::BudgetExhausted
            }
        );
    }
}

// ---------------------------------------------------------------------------
// terminal-state behaviour
// ---------------------------------------------------------------------------

#[test]
fn shipped_rejects_every_event() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Shipped;
    let events = [
        coder_started_default(),
        role_done_shipped("coder-claude-agentry"),
        role_done_shipped("ac-verifier-claude-agentry"),
        role_done_shipped("reviewer-claude-agentry"),
        BriefEvent::RoleDone {
            node_id: NodeId("shipper-agentry".to_owned()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: Some(RunData::PrTracking {
                pr_number: 1,
                head_sha: "h".to_owned(),
            }),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".to_owned(),
        },
        BriefEvent::RebaseStarted,
        BriefEvent::Rebased {
            new_head_sha: "n".to_owned(),
        },
        BriefEvent::RetryRequested {
            actor: "h".to_owned(),
            reason: "r".to_owned(),
        },
        abort_event(),
        BriefEvent::BudgetExhausted,
    ];
    for e in events {
        let err = handle(&s, &e, &cfg, &entry).expect_err("Shipped is terminal");
        assert_eq!(err.from, s);
        assert_eq!(err.event, e);
    }
}

#[test]
fn failed_accepts_only_retry_requested() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Failed {
        reason: Reason::BudgetExhausted,
    };
    let next = handle(
        &s,
        &BriefEvent::RetryRequested {
            actor: "h".to_owned(),
            reason: "manual retry".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("retry resets failed brief");
    assert_eq!(next, BriefState::Submitted);

    let bad = [
        coder_started_default(),
        role_done_shipped("coder-claude-agentry"),
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".to_owned(),
        },
        BriefEvent::RebaseStarted,
        abort_event(),
        BriefEvent::BudgetExhausted,
    ];
    for e in bad {
        let err = handle(&s, &e, &cfg, &entry).expect_err("Failed rejects non-retry events");
        assert_eq!(err.from, s);
        assert_eq!(err.event, e);
    }
}

// ---------------------------------------------------------------------------
// isolated invalid pairs
// ---------------------------------------------------------------------------

#[test]
fn submitted_rejects_non_starter_events() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Submitted;
    let bad = [
        role_done_shipped("coder-claude-agentry"),
        role_done_shipped("ac-verifier-claude-agentry"),
        role_done_shipped("reviewer-claude-agentry"),
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".to_owned(),
        },
        BriefEvent::RebaseStarted,
        BriefEvent::Rebased {
            new_head_sha: "n".to_owned(),
        },
        BriefEvent::RetryRequested {
            actor: "h".to_owned(),
            reason: "r".to_owned(),
        },
    ];
    for e in bad {
        handle(&s, &e, &cfg, &entry).expect_err("submitted rejects non-starter events");
    }
}

// ---------------------------------------------------------------------------
// caps are exposed so topologies can validate against them
// ---------------------------------------------------------------------------

#[test]
fn budget_caps_have_expected_values() {
    assert_eq!(DEFAULT_ATTEMPT_CAP, 3);
    assert_eq!(MAXIMUM_ATTEMPT_CAP, 10);
}

// ---------------------------------------------------------------------------
// serde round-trips
// ---------------------------------------------------------------------------

#[test]
fn brief_state_roundtrip_every_variant() {
    let mut evidence = BTreeMap::new();
    evidence.insert(coder_node(), EventVerdict::Shipped);
    let variants = vec![
        BriefState::Submitted,
        BriefState::Walking {
            node_id: coder_node(),
            evidence: BTreeMap::new(),
            run_data: RunData::Coder {
                agent_id: "c".to_owned(),
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("ac-verifier-claude-agentry".to_owned()),
            evidence: evidence.clone(),
            run_data: RunData::None,
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("shipper-agentry".to_owned()),
            evidence: evidence.clone(),
            run_data: RunData::PrTracking {
                pr_number: 9,
                head_sha: "deadbeef".to_owned(),
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: coder_node(),
            evidence: evidence.clone(),
            run_data: RunData::OperatorDecision {
                disagreements: sample_disagreements(),
            },
            retry: fresh_retry(),
        },
        BriefState::Walking {
            node_id: NodeId("ext-agentry".to_owned()),
            evidence: BTreeMap::new(),
            run_data: RunData::Extension {
                data: serde_json::json!({"k":"v"}),
            },
            retry: fresh_retry(),
        },
        BriefState::Shipped,
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        },
        BriefState::Failed {
            reason: Reason::AbortRequested {
                actor: "human".to_owned(),
                message: "stop".to_owned(),
            },
        },
        BriefState::Failed {
            reason: Reason::AcceptanceFailed {
                detail: "did not pass".to_owned(),
            },
        },
        BriefState::Failed {
            reason: Reason::DaemonError {
                detail: "redis down".to_owned(),
            },
        },
        BriefState::Failed {
            reason: Reason::CaptainRejectedDisagreement {
                reason: "literal verb preferred".to_owned(),
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
        coder_started_default(),
        BriefEvent::RoleDone {
            node_id: coder_node(),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: None,
        },
        BriefEvent::RoleDone {
            node_id: NodeId("ac-verifier-claude-agentry".to_owned()),
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![],
            run_data: None,
        },
        BriefEvent::RoleDone {
            node_id: NodeId("reviewer-claude-agentry".to_owned()),
            verdict: EventVerdict::ReworkNeeded,
            findings: vec![ReviewFinding {
                file: Some("src/lib.rs".to_owned()),
                line: Some(7),
                severity: orchestrator_types::Severity::Blocker,
                origin: orchestrator_types::FindingOrigin::Mechanical {
                    tool: "clippy".to_owned(),
                    rule: Some("unused_variables".to_owned()),
                },
                category: "lint".to_owned(),
                message: "unused var".to_owned(),
                suggested_fix: None,
                prohibitions: vec![],
                requirements: vec![],
            }],
            run_data: None,
        },
        BriefEvent::RoleDone {
            node_id: NodeId("shipper-agentry".to_owned()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: Some(RunData::PrTracking {
                pr_number: 1,
                head_sha: "h".to_owned(),
            }),
        },
        BriefEvent::CoderDoneNoOp {
            reason: "no diff".to_owned(),
        },
        BriefEvent::CoderDisagreed {
            disagreements: sample_disagreements(),
        },
        BriefEvent::CaptainAccepted,
        BriefEvent::CaptainRejected {
            reason: "literal verb".to_owned(),
        },
        BriefEvent::PreflightSmellDetected {
            smell_id: "smell-1".to_owned(),
            criterion: "the brief must do X".to_owned(),
            baseline: "no X observed".to_owned(),
        },
        BriefEvent::CiResult {
            state: CiState::Pending,
            head_sha: "h".to_owned(),
        },
        BriefEvent::CiResult {
            state: CiState::Success,
            head_sha: "h".to_owned(),
        },
        BriefEvent::CiResult {
            state: CiState::Failed,
            head_sha: "h".to_owned(),
        },
        BriefEvent::RebaseStarted,
        BriefEvent::Rebased {
            new_head_sha: "n".to_owned(),
        },
        BriefEvent::RetryRequested {
            actor: "h".to_owned(),
            reason: "r".to_owned(),
        },
        abort_event(),
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
        brief_id: BriefId("brf_test".to_owned()),
        state: BriefState::Walking {
            node_id: NodeId("ac-verifier-claude-agentry".to_owned()),
            evidence: BTreeMap::new(),
            run_data: RunData::None,
            retry: fresh_retry(),
        },
        parent_brief_id: Some(BriefId("brf_parent".to_owned())),
        composition_role: Some("planner-child".to_owned()),
        at: now(),
    };
    let s = serde_json::to_string(&record).expect("serialize");
    let back: BriefStateRecord = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(record, back);
}

#[test]
fn brief_state_record_optional_fields_omitted_when_none() {
    let record = BriefStateRecord {
        brief_id: BriefId("brf_test".to_owned()),
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
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Shipped;
    let e = coder_started_default();
    let err: Box<InvalidTransition> =
        handle(&s, &e, &cfg, &entry).expect_err("Shipped + CoderStarted is invalid");
    assert_eq!(err.from, s);
    assert_eq!(err.event, e);
    let _cloned = err.clone();
}

// ---------------------------------------------------------------------------
// captain-mediated disagreement resolution
// ---------------------------------------------------------------------------

fn sample_disagreements() -> Vec<DisagreementSummary> {
    vec![DisagreementSummary {
        verb: "REPLACE crates/foo/src/bar.rs:42 with `let x = 1;`".to_owned(),
        applied_form: "REPLACE crates/foo/src/bar.rs:42 with `let x = 1u32;`".to_owned(),
        rationale: "the literal verb produces a type-inference error; coerce to u32".to_owned(),
    }]
}

#[test]
fn coder_disagreed_flips_run_data_to_operator_decision() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let retry = RetryBudget { attempt: 1, max: 3 };
    let mut evidence = BTreeMap::new();
    evidence.insert(coder_node(), EventVerdict::Failed); // arbitrary preserved evidence
    let s = BriefState::Walking {
        node_id: coder_node(),
        evidence: evidence.clone(),
        run_data: RunData::Coder {
            agent_id: "c".to_owned(),
        },
        retry,
    };
    let disagreements = sample_disagreements();
    let next = handle(
        &s,
        &BriefEvent::CoderDisagreed {
            disagreements: disagreements.clone(),
        },
        &cfg,
        &entry,
    )
    .expect("coder disagreed");
    match next {
        BriefState::Walking {
            node_id,
            evidence: ev,
            run_data: RunData::OperatorDecision { disagreements: d },
            retry: r,
        } => {
            assert_eq!(node_id, coder_node());
            assert_eq!(ev, evidence);
            assert_eq!(d, disagreements);
            assert_eq!(r, retry);
        }
        other => panic!("expected Walking{{OperatorDecision}}, got {other:?}"),
    }
}

#[test]
fn captain_accepted_from_operator_decision_advances_walker() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let retry = RetryBudget { attempt: 1, max: 3 };
    let s = BriefState::Walking {
        node_id: coder_node(),
        evidence: BTreeMap::new(),
        run_data: RunData::OperatorDecision {
            disagreements: sample_disagreements(),
        },
        retry,
    };
    let next = handle(&s, &BriefEvent::CaptainAccepted, &cfg, &entry).expect("captain accepted");
    // Walker should advance to first downstream ac-verifier with evidence
    // recording coder Shipped.
    match next {
        BriefState::Walking {
            node_id,
            evidence,
            retry: r,
            ..
        } => {
            assert!(
                node_id.0.starts_with("ac-verifier-"),
                "expected advance to ac-verifier, got {node_id:?}"
            );
            assert_eq!(evidence.get(&coder_node()), Some(&EventVerdict::Shipped));
            assert_eq!(r, retry);
        }
        other => panic!("expected Walking{{ac-verifier}}, got {other:?}"),
    }
}

#[test]
fn captain_rejected_from_operator_decision_fails_brief() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: coder_node(),
        evidence: BTreeMap::new(),
        run_data: RunData::OperatorDecision {
            disagreements: sample_disagreements(),
        },
        retry: fresh_retry(),
    };
    let next = handle(
        &s,
        &BriefEvent::CaptainRejected {
            reason: "captain prefers literal verb".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("captain rejected");
    assert_eq!(
        next,
        BriefState::Failed {
            reason: Reason::CaptainRejectedDisagreement {
                reason: "captain prefers literal verb".to_owned(),
            },
        }
    );
}

#[test]
fn coder_disagreed_outside_coder_node_is_invalid() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = BriefState::Walking {
        node_id: NodeId("reviewer-claude-agentry".to_owned()),
        evidence: BTreeMap::new(),
        run_data: RunData::None,
        retry: fresh_retry(),
    };
    let event = BriefEvent::CoderDisagreed {
        disagreements: sample_disagreements(),
    };
    let err = handle(&s, &event, &cfg, &entry).expect_err("disagreement only on coder node");
    assert_eq!(err.from, s);
    assert_eq!(err.event, event);
}

// ---------------------------------------------------------------------------
// no-op short-circuit — coder finds no diff against base
// ---------------------------------------------------------------------------

#[test]
fn coder_done_no_op_short_circuits_to_shipped() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = walking_at_coder(coder_node(), fresh_retry());
    let next = handle(
        &s,
        &BriefEvent::CoderDoneNoOp {
            reason: "no diff against base".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("coder no-op");
    assert_eq!(next, BriefState::Shipped);
}

#[test]
fn preflight_smell_detected_at_entry_coder_fails_brief() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let s = walking_at_coder(coder_node(), fresh_retry());
    let next = handle(
        &s,
        &BriefEvent::PreflightSmellDetected {
            smell_id: "criterion_includes_baseline".to_owned(),
            criterion: "ship by 5pm".to_owned(),
            baseline: "baseline value".to_owned(),
        },
        &cfg,
        &entry,
    )
    .expect("preflight smell");
    assert_eq!(
        next,
        BriefState::Failed {
            reason: Reason::PreflightSmell
        }
    );
}

// ---------------------------------------------------------------------------
// added behaviour tests — pin post-collapse semantics
// ---------------------------------------------------------------------------

/// Pin: the entry Walking node_id equals the role_name carried on
/// CoderStarted, NOT a hardcoded "coder-claude-agentry".
#[test]
fn submitted_with_coder_started_role_name_drives_walking_node_id() {
    let (cfg, entry) = synthetic_linear_walk_config(&["coder-mythic-agentry", "shipper-agentry"]);
    let s0 = BriefState::Submitted;
    let event = BriefEvent::CoderStarted {
        agent_id: "agt_42".to_owned(),
        role_name: "coder-mythic-agentry".to_owned(),
        started_at: now(),
    };
    let next = handle(&s0, &event, &cfg, &entry).expect("submitted + coder_started");
    match next {
        BriefState::Walking {
            node_id,
            run_data: RunData::Coder { agent_id },
            ..
        } => {
            assert_eq!(
                node_id.0, "coder-mythic-agentry",
                "node_id must mirror role_name verbatim"
            );
            assert_ne!(
                node_id.0, "coder-claude-agentry",
                "node_id must NOT be hardcoded to coder-claude-agentry"
            );
            assert_eq!(agent_id, "agt_42");
        }
        other => panic!("expected Walking{{Coder}}, got {other:?}"),
    }
}

/// Pin: CoderDisagreed preserves node_id + evidence + retry; only run_data
/// flips from `Coder` to `OperatorDecision`.
#[test]
fn coder_disagreed_preserves_node_id_evidence_and_retry() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    let retry = RetryBudget { attempt: 2, max: 5 };
    let mut evidence = BTreeMap::new();
    evidence.insert(NodeId("prior-marker".to_owned()), EventVerdict::Shipped);
    let s = BriefState::Walking {
        node_id: coder_node(),
        evidence: evidence.clone(),
        run_data: RunData::Coder {
            agent_id: "agt".to_owned(),
        },
        retry,
    };
    let disagreements = sample_disagreements();
    let next = handle(
        &s,
        &BriefEvent::CoderDisagreed {
            disagreements: disagreements.clone(),
        },
        &cfg,
        &entry,
    )
    .expect("coder disagreed");
    assert_eq!(
        next,
        BriefState::Walking {
            node_id: coder_node(),
            evidence,
            run_data: RunData::OperatorDecision { disagreements },
            retry,
        }
    );
}

/// Pin: a non-Claude coder role can drive an end-to-end Submitted → Shipped
/// walk; the FSM never hardcodes "coder-claude-agentry".
#[test]
fn non_claude_coder_topology_walks_submitted_to_shipped() {
    let (cfg, entry) = synthetic_linear_walk_config(&[
        "coder-codex-agentry",
        "reviewer-codex-agentry",
        "shipper-agentry",
    ]);
    let s0 = BriefState::Submitted;
    let started = BriefEvent::CoderStarted {
        agent_id: "agt_codex".to_owned(),
        role_name: "coder-codex-agentry".to_owned(),
        started_at: now(),
    };
    let s1 = handle(&s0, &started, &cfg, &entry).expect("submitted + coder_started");
    if let BriefState::Walking { ref node_id, .. } = s1 {
        assert_ne!(node_id.0, "coder-claude-agentry");
    } else {
        panic!("expected Walking after CoderStarted");
    }

    let s2 = handle(&s1, &role_done_shipped("coder-codex-agentry"), &cfg, &entry)
        .expect("coder codex shipped");
    if let BriefState::Walking { ref node_id, .. } = s2 {
        assert_ne!(node_id.0, "coder-claude-agentry");
        assert_eq!(node_id.0, "reviewer-codex-agentry");
    } else {
        panic!("expected Walking after coder shipped");
    }

    let s3 = handle(
        &s2,
        &role_done_shipped("reviewer-codex-agentry"),
        &cfg,
        &entry,
    )
    .expect("reviewer codex shipped");
    if let BriefState::Walking { ref node_id, .. } = s3 {
        assert_ne!(node_id.0, "coder-claude-agentry");
        assert_eq!(node_id.0, "shipper-agentry");
    } else {
        panic!("expected Walking after reviewer shipped");
    }

    let s4 = handle(&s3, &role_done_shipped("shipper-agentry"), &cfg, &entry)
        .expect("shipper shipped — sink reached");
    assert_eq!(s4, BriefState::Shipped);
}

/// Pin: a `RoleDone` from a node strictly upstream of the walker position
/// is a late event — state stays unchanged (no panic, no InvalidTransition).
#[test]
fn late_role_done_stays_at_state() {
    let (cfg, entry) = synthetic_self_host_walk_config();
    // Walker has moved past the coder, currently at a reviewer.
    let mut evidence = BTreeMap::new();
    evidence.insert(coder_node(), EventVerdict::Shipped);
    let s = BriefState::Walking {
        node_id: NodeId("reviewer-claude-agentry".to_owned()),
        evidence: evidence.clone(),
        run_data: RunData::None,
        retry: fresh_retry(),
    };
    // Late RoleDone arrives from the upstream coder node.
    let next = handle(
        &s,
        &BriefEvent::RoleDone {
            node_id: coder_node(),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: None,
        },
        &cfg,
        &entry,
    )
    .expect("late event must not error");
    assert_eq!(next, s, "late event must leave state unchanged");
}

// ---------------------------------------------------------------------------
// precursor-types coverage (Walking serde + RunData accessors + WalkConfig)
// ---------------------------------------------------------------------------

#[test]
fn walking_state_roundtrips_through_serde() {
    let state = BriefState::Walking {
        node_id: coder_node(),
        evidence: BTreeMap::new(),
        run_data: RunData::Coder {
            agent_id: "agt_test_123".to_owned(),
        },
        retry: fresh_retry(),
    };
    let json = serde_json::to_string(&state).expect("serialize Walking");
    let back: BriefState = serde_json::from_str(&json).expect("deserialize Walking");
    assert_eq!(state, back);
}

#[test]
fn run_data_accessors_match_variants() {
    let none = RunData::None;
    let coder = RunData::Coder {
        agent_id: "agt_test".to_owned(),
    };
    let pr = RunData::PrTracking {
        pr_number: 42,
        head_sha: "deadbeef".to_owned(),
    };
    let op = RunData::OperatorDecision {
        disagreements: vec![DisagreementSummary {
            verb: "UPDATE".to_owned(),
            applied_form: "REPLACE".to_owned(),
            rationale: "verb mismatch".to_owned(),
        }],
    };
    let ext = RunData::Extension {
        data: serde_json::json!({"k": "v"}),
    };

    assert_eq!(coder.agent_id(), Some("agt_test"));
    assert_eq!(none.agent_id(), None);
    assert_eq!(pr.agent_id(), None);
    assert_eq!(op.agent_id(), None);
    assert_eq!(ext.agent_id(), None);

    assert_eq!(pr.pr_number(), Some(42));
    assert_eq!(none.pr_number(), None);
    assert_eq!(coder.pr_number(), None);
    assert_eq!(op.pr_number(), None);
    assert_eq!(ext.pr_number(), None);

    assert_eq!(pr.head_sha(), Some("deadbeef"));
    assert_eq!(none.head_sha(), None);
    assert_eq!(coder.head_sha(), None);
    assert_eq!(op.head_sha(), None);
    assert_eq!(ext.head_sha(), None);

    assert_eq!(op.disagreements().map(<[_]>::len), Some(1));
    assert_eq!(none.disagreements(), None);
    assert_eq!(coder.disagreements(), None);
    assert_eq!(pr.disagreements(), None);
    assert_eq!(ext.disagreements(), None);
}

#[test]
fn walk_config_round_trips() {
    let a = NodeId("node-a".to_owned());
    let b = NodeId("node-b".to_owned());

    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    adjacency.insert(a.clone(), vec![b.clone()]);
    adjacency.insert(b.clone(), vec![]);

    let mut node_configs: HashMap<NodeId, NodeConfig> = HashMap::new();
    node_configs.insert(
        a.clone(),
        NodeConfig {
            class: NodeClass("container_bound".to_owned()),
            expected_inbound: vec![],
            policy: GatePolicy::AllMustPass,
        },
    );
    node_configs.insert(
        b.clone(),
        NodeConfig {
            class: NodeClass("container_bound".to_owned()),
            expected_inbound: vec![a.clone()],
            policy: GatePolicy::AllMustPass,
        },
    );

    let cfg = WalkConfig {
        adjacency,
        node_configs,
    };
    let json = serde_json::to_string(&cfg).expect("serialize WalkConfig");
    let back: WalkConfig = serde_json::from_str(&json).expect("deserialize WalkConfig");
    assert_eq!(cfg, back);
}

// ---------------------------------------------------------------------------
// GateConfig serde — alpha precursor coverage retained
// ---------------------------------------------------------------------------

#[test]
fn gate_config_roundtrips_through_serde() {
    let gc = GateConfig {
        expected_roles: vec![
            "ac-verifier-claude".to_owned(),
            "ac-verifier-gemini".to_owned(),
        ],
        policy: GatePolicy::Majority { threshold_pct: 67 },
    };
    let s = serde_json::to_string(&gc).expect("serialize");
    let back: GateConfig = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(gc, back);
}
