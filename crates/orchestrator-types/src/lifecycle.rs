//! Brief lifecycle state machine — pure types and transition function.
//!
//! Implements the FSM described in `specs/concepts/brief_lifecycle.md` and
//! the retry budget mechanics in `specs/concepts/brief_retry_budget.md`.
//!
//! `handle` is a pure function of `(state, event)`. Wall-clock time and
//! brief-id wrapping are layered by the daemon caller (see L.2).

use crate::run_data::RunData;
use crate::team::NodeId;
use crate::{BriefId, EventVerdict, ReviewFinding, Ts};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Compile-time default for `RetryBudget.max` when a topology does not
/// specify `max_retries`.
pub const DEFAULT_ATTEMPT_CAP: u32 = 3;

/// Compile-time hard ceiling. Topologies declaring `max_retries` above
/// this are rejected at dispatch with `Reason::AcceptanceFailed`.
pub const MAXIMUM_ATTEMPT_CAP: u32 = 10;

/// Persisted projection of a brief's current lifecycle position. The daemon
/// writes one of these per FSM step; the projector replays them on resume.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BriefStateRecord {
    pub brief_id: BriefId,
    pub state: BriefState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_brief_id: Option<BriefId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition_role: Option<String>,
    pub at: Ts,
}

/// Per-state retry counter. `attempt` is 1-based: the first authoring run
/// is `attempt=1`, the first rework is `attempt=2`. `max` is the hard cap;
/// when `attempt > max` the FSM short-circuits to `Failed{BudgetExhausted}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryBudget {
    pub attempt: u32,
    pub max: u32,
}

/// Why a brief landed in `BriefState::Failed`. Tagged so dashboards can
/// distinguish budget-exhaustion from human aborts from acceptance gate
/// failures from daemon-internal errors without parsing free text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    BudgetExhausted,
    AbortRequested {
        actor: String,
        message: String,
    },
    AcceptanceFailed {
        detail: String,
    },
    /// preflight-criterion-agentry detected a smell heuristic firing on the
    /// brief's `success_criteria`. The criterion is operator-authored and
    /// the smell rules ARE the contract — refining is a code-level change
    /// to the heuristics, not an operator-overridable knob. Smell details
    /// (which smell-id fired, criterion text, baseline value) ride in the
    /// `BriefEvent::PreflightSmellDetected` payload that triggers the
    /// transition; the variant itself carries no payload so dashboards
    /// surface a typed badge without parsing prose.
    PreflightSmell,
    DaemonError {
        detail: String,
    },
    /// Fires when the daemon's boot-time resume scan finds a brief in a
    /// non-terminal `:state` whose named container is no longer alive.
    DaemonRestartedDuringExecution,
    /// Captain rejected a coder-flagged disagreement via captain decide reject.
    /// The reason carries the captain's prose explanation (audit trail).
    CaptainRejectedDisagreement {
        reason: String,
    },
    /// `build_walk_config` could not derive a unique entry vertex from the
    /// topology (zero or more than one node with empty `expected_inbound`).
    /// A topology-data bug — the brief fails with this so the operator
    /// fixes the topology rather than silently routing through an
    /// arbitrary node.
    TopologyInvalid {
        detail: String,
    },
}

/// CI status carried by a `BriefEvent::CiResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    Pending,
    Success,
    Failed,
}

/// Which role re-runs when a rework loop kicks off — coder produces fresh
/// changes, reviewer re-runs the deterministic gate against the same diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReworkTarget {
    Coder,
    Reviewer,
}

/// One coder-flagged disagreement with a brief verb. The coder produced
/// applied_form (the variant it actually emitted) and rationale (why)
/// instead of the literal verb. F6a (PR #443) added these fields to
/// the role-runtime UnappliedVerb shape; F6b (this brief + 449b) lifts
/// them into orchestrator-types so the FSM can carry disagreements
/// across phases without a role-runtime dependency. Wire-equivalent
/// to UnappliedVerb at the JSON level.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DisagreementSummary {
    pub verb: String,
    #[serde(default)]
    pub applied_form: String,
    #[serde(default)]
    pub rationale: String,
}

/// The position a brief occupies inside the lifecycle FSM.
///
/// Post-#495-beta-b: collapsed to 4 variants. `Walking` carries the
/// brief's position inside the team-topology DAG — the node whose role
/// just reported (`node_id`), the accumulated multiset of per-node
/// verdicts (`evidence`), per-node run data (`run_data` — coder agent
/// id, PR tracking, or operator-decision park payload), and the retry
/// budget. `Submitted` is the pre-walk state before any role spawns;
/// `Shipped` and `Failed` are the two terminals.
///
/// The legacy phase-specific variants (`Authoring`, `Verifying`,
/// `Reviewing`, `Reworking`, `Shipping`, `Watching`, `Extension`,
/// `AwaitingCaptainDecision`) were deleted in beta-b — phase names are
/// now metadata on the topology, not enum variants. See
/// `specs/concepts/brief_lifecycle.md` for the post-collapse doctrine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BriefState {
    Submitted,
    /// Per-node lifecycle position. `node_id` is the role-name of the
    /// node whose most recent `RoleDone` shaped the current state — also
    /// used by the projector as the late-event fence reference (events
    /// for nodes already passed by the walker are dropped with a
    /// tracing warn, not propagated as `InvalidTransition`).
    ///
    /// `evidence` accumulates `(NodeId → EventVerdict)` across the
    /// whole walk; the projector consults it on every `RoleDone` to
    /// decide whether each downstream node's gate (per `WalkConfig`)
    /// is satisfied.
    ///
    /// `run_data` carries the per-node variant: `Coder { agent_id }`
    /// while a coder container is alive, `PrTracking { pr_number,
    /// head_sha }` from the shipper onward, `OperatorDecision
    /// { disagreements }` when a coder reported a deliberate
    /// disagreement and the brief is parked for captain decide, or
    /// `None` for stateless nodes.
    Walking {
        node_id: NodeId,
        evidence: BTreeMap<NodeId, EventVerdict>,
        run_data: RunData,
        retry: RetryBudget,
    },
    Shipped,
    Failed {
        reason: Reason,
    },
}

/// Inputs to the FSM. Each variant corresponds to one observable signal
/// the daemon harvests from agent stdout, the gitea poller, or a human
/// command-channel message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BriefEvent {
    CoderStarted {
        agent_id: String,
        /// The full role name as emitted by the spawner (e.g.
        /// `"coder-claude-agentry"`, `"coder-codex-agentry"`). Carried so
        /// the FSM can construct `BriefState::Walking { node_id }` keyed
        /// off the topology-declared role name rather than a hardcoded
        /// literal. The translator copies this from the `spawned` event's
        /// `role_name` payload at `lifecycle_ports.rs`'s coder dispatch.
        #[serde(default)]
        role_name: String,
        #[serde(default = "crate::now")]
        started_at: Ts,
    },
    /// Coder reported terminal Shipped but produced no diff against base
    /// (acceptance passed against work that was already on the base
    /// branch). Short-circuits the FSM `Walking{coder} → Shipped`,
    /// bypassing every downstream node since there is nothing for them
    /// to operate on. The free-text reason carries the coder's diagnosis
    /// for the operator-visible terminal verdict.
    CoderDoneNoOp {
        reason: String,
    },
    /// Self-review found unapplied verbs but every miss has applied_form+rationale
    /// set. Coder is flagging a deliberate disagreement, not a failure. The FSM
    /// flips the coder's `Walking{run_data: Coder{..}}` to
    /// `Walking{run_data: OperatorDecision{disagreements}}` and the brief is
    /// parked until CaptainAccepted / CaptainRejected fires.
    CoderDisagreed {
        disagreements: Vec<DisagreementSummary>,
    },
    CiResult {
        state: CiState,
        head_sha: String,
    },
    RebaseStarted,
    Rebased {
        new_head_sha: String,
    },
    RetryRequested {
        actor: String,
        reason: String,
    },
    AbortRequested {
        actor: String,
        message: String,
    },
    /// Captain endorses the disagreed-form output. The FSM treats the brief
    /// as if the coder had shipped normally — advances the walker from
    /// the coder node to its downstream node(s) using the work that's
    /// already in the brief workspace. The current `Walking.evidence`
    /// records the coder's contribution as `EventVerdict::Shipped` so
    /// the next gate sees a clean inbound.
    CaptainAccepted,
    /// Captain explicitly rejects the disagreement. The FSM transitions to
    /// `Failed{CaptainRejectedDisagreement{reason}}`.
    CaptainRejected {
        reason: String,
    },
    BudgetExhausted,
    /// preflight-criterion-agentry's smell heuristics fired on the brief's
    /// `success_criteria`. The runner emits `done failed` with
    /// `cause: "preflight_smell"`; the daemon's trace translator (wired in
    /// 84b-2) folds that into this variant so the FSM can transition the
    /// brief to `Failed { reason: PreflightSmell }`. Carries the smell
    /// identifier plus the criterion text and observed baseline so the
    /// terminal verdict surfaces enough context for the operator to
    /// rewrite the criterion without re-reading the trace stream.
    PreflightSmellDetected {
        smell_id: String,
        criterion: String,
        baseline: String,
    },
    /// Per-node done signal — the single generic role-completion event
    /// after beta-b's collapse. The translator emits this for every
    /// `EventKind::Done` regardless of role (coder, ac-verifier,
    /// reviewer, shipper, ci-watcher). Carries the source node id, the
    /// node's verdict, any review findings the role produced, and an
    /// optional `RunData` payload. The shipper's emit carries
    /// `Some(RunData::PrTracking { pr_number, head_sha })` so the
    /// walker's next state (ci-watcher node) inherits PR identifiers;
    /// other roles emit `run_data: None`.
    RoleDone {
        node_id: NodeId,
        verdict: EventVerdict,
        findings: Vec<ReviewFinding>,
        run_data: Option<RunData>,
    },
}

/// Returned when an event is not legal in the current state. Carries an
/// owned snapshot of both so the caller can log or surface the bad pair
/// without re-borrowing.
#[derive(Debug, Clone, PartialEq)]
pub struct InvalidTransition {
    pub from: BriefState,
    pub event: BriefEvent,
}

/// Map an agent's full role name (as emitted by the spawner — e.g.
/// `"coder-claude-agentry"`, `"shipper-agentry"`,
/// `"preflight-criterion-agentry"`) to the short role kind the
/// translator's `Done`-branch matches against.
///
/// Returns `None` for role names outside the recognised families; the
/// translator skips memoization in that case so the `Done` lookup
/// falls through to the catch-all (no `BriefEvent` emitted) rather
/// than mis-classifying an unknown role.
///
/// Public so the peer `tests/lifecycle.rs` integration suite can pin
/// the per-family mapping without re-deriving the prefix table.
#[must_use]
pub fn role_kind(role_name: &str) -> Option<&'static str> {
    if role_name.starts_with("coder-") {
        Some("coder")
    } else if role_name.starts_with("ac-verifier-") {
        Some("ac-verifier")
    } else if role_name.starts_with("verifier-") {
        Some("verifier")
    } else if role_name.starts_with("reviewer-") {
        Some("reviewer")
    } else if role_name == "shipper-agentry" {
        Some("shipper")
    } else if role_name == "ci-watcher-agentry" {
        Some("ci-watcher")
    } else if role_name.starts_with("preflight-criterion") {
        Some("preflight")
    } else {
        None
    }
}

/// Pure transition function. Returns the new state for a valid transition,
/// or `InvalidTransition` for an event that is not allowed in the current
/// state. Never panics, never awaits, never performs I/O.
///
/// Post-#495-beta-b shape: drives a generic topology-walker. Inputs are
/// the current `BriefState`, the next `BriefEvent`, the precomputed
/// `WalkConfig` for the brief's team topology (adjacency + per-node
/// gate config), and `entry_node` (the unique topology root — the node
/// whose `expected_inbound` is empty; the projector computes it once
/// from `WalkConfig` and threads it here on every call).
///
/// Retry-budget contract: when a transition would push `attempt > max`
/// on a non-terminal state, the function returns `Failed{BudgetExhausted}`
/// instead of the proposed next state.
///
/// Error type is boxed because [`InvalidTransition`] embeds `BriefState +
/// BriefEvent`, both of which grow whenever a new variant lands; clippy's
/// `result_large_err` lint (denied as error in CI) flags the unboxed
/// shape once the inner pair crosses the 128-byte threshold.
pub fn handle(
    state: &BriefState,
    event: &BriefEvent,
    walk_config: &WalkConfig,
    entry_node: &NodeId,
) -> Result<BriefState, Box<InvalidTransition>> {
    let invalid = || {
        Err(Box::new(InvalidTransition {
            from: state.clone(),
            event: event.clone(),
        }))
    };

    // Universal aborts on every non-terminal state.
    if !matches!(state, BriefState::Shipped | BriefState::Failed { .. }) {
        match event {
            BriefEvent::AbortRequested { actor, message } => {
                return Ok(BriefState::Failed {
                    reason: Reason::AbortRequested {
                        actor: actor.clone(),
                        message: message.clone(),
                    },
                });
            }
            BriefEvent::BudgetExhausted => {
                return Ok(BriefState::Failed {
                    reason: Reason::BudgetExhausted,
                });
            }
            _ => {}
        }
    }

    match (state, event) {
        // ---- Submitted ----
        // First coder spawn: enter Walking at the entry node. The
        // node_id comes from the event's role_name (the spawner-emitted
        // role identifier) — DO NOT hardcode any coder role name here.
        // The entry_node arg is the topology root (computed by the
        // projector from WalkConfig.adjacency) and must equal
        // NodeId(role_name) for a well-formed dispatch; if it doesn't,
        // that's a topology-vs-spawn mismatch and we trust the event
        // since it reflects the actually-spawned role.
        (
            BriefState::Submitted,
            BriefEvent::CoderStarted {
                agent_id,
                role_name,
                ..
            },
        ) => Ok(BriefState::Walking {
            node_id: NodeId(role_name.clone()),
            evidence: BTreeMap::new(),
            run_data: RunData::Coder {
                agent_id: agent_id.clone(),
            },
            retry: RetryBudget {
                attempt: 1,
                max: DEFAULT_ATTEMPT_CAP,
            },
        }),

        // ---- Walking + preflight smell (only at the entry coder node) ----
        (
            BriefState::Walking {
                node_id,
                run_data: RunData::Coder { .. },
                ..
            },
            BriefEvent::PreflightSmellDetected { .. },
        ) if node_id == entry_node => Ok(BriefState::Failed {
            reason: Reason::PreflightSmell,
        }),

        // ---- Walking + no-op short-circuit (only at the entry coder node) ----
        // Acceptance passed against work already on base; skip every
        // downstream node since there's nothing to operate on.
        (
            BriefState::Walking {
                node_id,
                run_data: RunData::Coder { .. },
                ..
            },
            BriefEvent::CoderDoneNoOp { .. },
        ) if node_id == entry_node => Ok(BriefState::Shipped),

        // ---- Walking + CoderDisagreed (only at coder node with Coder run_data) ----
        // Flip the run_data to OperatorDecision; keep the node_id and
        // evidence intact. The brief is now parked awaiting captain decide.
        (
            BriefState::Walking {
                node_id,
                evidence,
                run_data: RunData::Coder { .. },
                retry,
            },
            BriefEvent::CoderDisagreed { disagreements },
        ) => Ok(BriefState::Walking {
            node_id: node_id.clone(),
            evidence: evidence.clone(),
            run_data: RunData::OperatorDecision {
                disagreements: disagreements.clone(),
            },
            retry: *retry,
        }),

        // ---- Walking + CaptainAccepted (only with OperatorDecision run_data) ----
        // Treat as if the coder had shipped: record Shipped in evidence
        // and advance the walker. The agent_id is no longer known
        // (operator-decision park is post-coder-exit), so run_data resets
        // to None on the next node.
        (
            BriefState::Walking {
                node_id,
                evidence,
                run_data: RunData::OperatorDecision { .. },
                retry,
            },
            BriefEvent::CaptainAccepted,
        ) => {
            let mut new_evidence = evidence.clone();
            new_evidence.insert(node_id.clone(), EventVerdict::Shipped);
            advance_walker(
                node_id,
                new_evidence,
                RunData::None,
                *retry,
                walk_config,
                entry_node,
            )
        }

        // ---- Walking + CaptainRejected ----
        (
            BriefState::Walking {
                run_data: RunData::OperatorDecision { .. },
                ..
            },
            BriefEvent::CaptainRejected { reason },
        ) => Ok(BriefState::Failed {
            reason: Reason::CaptainRejectedDisagreement {
                reason: reason.clone(),
            },
        }),

        // ---- Walking + RoleDone (the universal node-completion event) ----
        // Update evidence with the just-reported node's verdict, then
        // advance the walker based on adjacency + per-node gates.
        // Late-event check: if the reporting node is upstream of the
        // current walker position (i.e., already passed), drop silently
        // (return state unchanged). The lifecycle_driver caller is
        // expected to emit a tracing::warn for the dropped event but
        // does so by detecting state == new_state with event = RoleDone.
        (
            BriefState::Walking {
                node_id,
                evidence,
                run_data,
                retry,
            },
            BriefEvent::RoleDone {
                node_id: reporter,
                verdict,
                findings: _,
                run_data: rd_payload,
            },
        ) => {
            if is_late_event(reporter, node_id, walk_config) {
                return Ok(BriefState::Walking {
                    node_id: node_id.clone(),
                    evidence: evidence.clone(),
                    run_data: run_data.clone(),
                    retry: *retry,
                });
            }
            let mut new_evidence = evidence.clone();
            new_evidence.insert(reporter.clone(), *verdict);
            let inherited_run_data = rd_payload.clone().unwrap_or(RunData::None);
            match verdict {
                EventVerdict::Shipped => advance_walker(
                    reporter,
                    new_evidence,
                    inherited_run_data,
                    *retry,
                    walk_config,
                    entry_node,
                ),
                EventVerdict::ReworkNeeded | EventVerdict::Failed => {
                    Ok(increment_or_fail(*retry, |next| BriefState::Walking {
                        node_id: entry_node.clone(),
                        evidence: BTreeMap::new(),
                        run_data: RunData::None,
                        retry: next,
                    }))
                }
                EventVerdict::Escalated => Ok(BriefState::Failed {
                    reason: Reason::AcceptanceFailed {
                        detail: format!("{} escalated", reporter.0),
                    },
                }),
                EventVerdict::Rejected => Ok(BriefState::Failed {
                    reason: Reason::AcceptanceFailed {
                        detail: format!("{} rejected", reporter.0),
                    },
                }),
            }
        }

        // ---- Walking + CoderStarted (rework re-spawn at entry node) ----
        (
            BriefState::Walking { retry, .. },
            BriefEvent::CoderStarted {
                agent_id,
                role_name,
                ..
            },
        ) if NodeId(role_name.clone()) == *entry_node => Ok(BriefState::Walking {
            node_id: entry_node.clone(),
            evidence: BTreeMap::new(),
            run_data: RunData::Coder {
                agent_id: agent_id.clone(),
            },
            retry: *retry,
        }),

        // ---- Walking + CiResult ----
        // CI watcher reports success/failure/pending. Success → terminal
        // Shipped; failed → rewind to entry (or BudgetExhausted);
        // pending → stay. We accept any run_data shape — when PrTracking
        // is plumbed end-to-end (B7 follow-up), the run_data carries
        // the head_sha and the daemon's rebase plumbing uses it; until
        // then we ignore the run_data variant on the CiResult arm.
        (BriefState::Walking { retry, .. }, BriefEvent::CiResult { state: ci, .. }) => match ci {
            CiState::Success => Ok(BriefState::Shipped),
            CiState::Failed => Ok(increment_or_fail(*retry, |next| BriefState::Walking {
                node_id: entry_node.clone(),
                evidence: BTreeMap::new(),
                run_data: RunData::None,
                retry: next,
            })),
            CiState::Pending => Ok(state.clone()),
        },

        // ---- Walking + RebaseStarted (no state change) ----
        (BriefState::Walking { .. }, BriefEvent::RebaseStarted) => Ok(state.clone()),

        // ---- Walking + Rebased: update head_sha when run_data carries
        // PrTracking; otherwise no-op stay.
        (
            BriefState::Walking {
                node_id,
                evidence,
                run_data:
                    RunData::PrTracking {
                        pr_number,
                        head_sha: _,
                    },
                retry,
            },
            BriefEvent::Rebased { new_head_sha },
        ) => Ok(BriefState::Walking {
            node_id: node_id.clone(),
            evidence: evidence.clone(),
            run_data: RunData::PrTracking {
                pr_number: *pr_number,
                head_sha: new_head_sha.clone(),
            },
            retry: *retry,
        }),
        (BriefState::Walking { .. }, BriefEvent::Rebased { .. }) => Ok(state.clone()),

        // ---- Failed + RetryRequested (operator-driven retry) ----
        (BriefState::Failed { .. }, BriefEvent::RetryRequested { .. }) => Ok(BriefState::Submitted),

        // ---- Everything else: not allowed in this state. ----
        _ => invalid(),
    }
}

/// Increment a `RetryBudget` and either build the proposed state with the
/// new budget, or short-circuit to `Failed{BudgetExhausted}` when the
/// increment would breach `max`.
fn increment_or_fail(
    retry: RetryBudget,
    build: impl FnOnce(RetryBudget) -> BriefState,
) -> BriefState {
    let next = RetryBudget {
        attempt: retry.attempt.saturating_add(1),
        max: retry.max,
    };
    if next.attempt > next.max {
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        }
    } else {
        build(next)
    }
}

/// Walker advance: called after `evidence[reporter] = verdict` was
/// recorded. For each downstream of `reporter` in the adjacency, check
/// whether that downstream's gate (expected_inbound + policy) is
/// satisfied by the new evidence. Advance to the first downstream
/// whose gate Passes; on Wait/Reject/Rework, apply the canonical
/// response without advancing.
///
/// `inherited_run_data` is the `run_data` payload the reporter's
/// `RoleDone` event carried (e.g. `Some(PrTracking{..})` from the
/// shipper). It is propagated to the next node's `Walking` state only
/// when the next node's `class` opts in (here we treat any non-None
/// inherited payload as opt-in; the runtime convention is that
/// shippers and shipper-like roles emit non-None payloads and only
/// PrTracking-consumer nodes should be downstream of them).
///
/// If the reporter has no downstreams in adjacency, the walk has
/// reached a sink — terminal `Shipped`.
fn advance_walker(
    reporter: &NodeId,
    new_evidence: BTreeMap<NodeId, EventVerdict>,
    inherited_run_data: RunData,
    retry: RetryBudget,
    walk_config: &WalkConfig,
    entry_node: &NodeId,
) -> Result<BriefState, Box<InvalidTransition>> {
    let downstreams = walk_config
        .adjacency
        .get(reporter)
        .cloned()
        .unwrap_or_default();

    if downstreams.is_empty() {
        return Ok(BriefState::Shipped);
    }

    for d in &downstreams {
        let Some(node_cfg) = walk_config.node_configs.get(d) else {
            continue;
        };
        let gate = GateConfig {
            expected_roles: node_cfg
                .expected_inbound
                .iter()
                .map(|n| n.0.clone())
                .collect(),
            policy: node_cfg.policy.clone(),
        };
        let received_for_d: BTreeMap<String, EventVerdict> = new_evidence
            .iter()
            .filter(|(k, _)| node_cfg.expected_inbound.contains(k))
            .map(|(k, v)| (k.0.clone(), *v))
            .collect();
        match decide(&received_for_d, &gate) {
            Decide::Pass => {
                let next_run_data = match &inherited_run_data {
                    RunData::PrTracking { .. } => inherited_run_data.clone(),
                    _ => RunData::None,
                };
                return Ok(BriefState::Walking {
                    node_id: d.clone(),
                    evidence: new_evidence,
                    run_data: next_run_data,
                    retry,
                });
            }
            Decide::Wait => continue,
            Decide::Rework { detail: _ } => {
                return Ok(increment_or_fail(retry, |next| BriefState::Walking {
                    node_id: entry_node.clone(),
                    evidence: BTreeMap::new(),
                    run_data: RunData::None,
                    retry: next,
                }));
            }
            Decide::Reject { detail } => {
                return Ok(BriefState::Failed {
                    reason: Reason::AcceptanceFailed { detail },
                });
            }
        }
    }

    // No downstream advanced — stay at reporter with the updated
    // evidence and inherited run_data so a future RoleDone for one of
    // reporter's siblings can complete the gate.
    Ok(BriefState::Walking {
        node_id: reporter.clone(),
        evidence: new_evidence,
        run_data: inherited_run_data,
        retry,
    })
}

/// Late-event fence: a `RoleDone` for `reporter` while the walker's
/// current position is `current` is a "late event" when `reporter` is
/// strictly upstream of `current` in the adjacency (i.e., the walker
/// has already moved past it). In that case `handle()` returns the
/// state unchanged and the lifecycle_driver logs a warn.
///
/// Implementation: reachability from `reporter` to `current` in the
/// adjacency-forward direction. If `current` is reachable from
/// `reporter` (and they're not equal), `reporter` is upstream → late.
fn is_late_event(reporter: &NodeId, current: &NodeId, walk_config: &WalkConfig) -> bool {
    if reporter == current {
        return false;
    }
    // BFS forward from reporter; if we reach current, reporter is upstream.
    let mut frontier: Vec<NodeId> = walk_config
        .adjacency
        .get(reporter)
        .cloned()
        .unwrap_or_default();
    let mut visited: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    while let Some(n) = frontier.pop() {
        if &n == current {
            return true;
        }
        if !visited.insert(n.clone()) {
            continue;
        }
        if let Some(adj) = walk_config.adjacency.get(&n) {
            frontier.extend(adj.iter().cloned());
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Additive gate-policy precursor (#396a) — types + pure `decide()`.
//
// These items are introduced ahead of the #396 FSM evidence migration. Today
// `handle()` transitions on the first matching event for each phase, which
// silently drops the 2nd and 3rd reports when a topology fans out multiple
// ac-verifiers or reviewers — a correctness bug, not just observability. The
// migration in 396b will replace the serial first-event arms in `Verifying`
// and `Reviewing` with evidence-based gating: BriefState carries the
// `received` verdict multiset and the per-phase `GateConfig`, and `handle()`
// only advances when `decide()` returns `Pass`. This brief lands the new
// types and the pure decision function additively; nothing in `handle()` or
// `BriefState` changes here.
// ---------------------------------------------------------------------------

/// The rule applied at a phase fan-in to fold a multiset of role verdicts
/// into a single `Decide` outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GatePolicy {
    AllMustPass,
    FailFast,
    Majority { threshold_pct: u32 },
}

/// Pairs a `GatePolicy` with the list of role-kinds the gate waits on.
/// `expected_roles` holds the role-kind strings as returned by
/// `lifecycle::role_kind` (e.g. `"ac-verifier-claude"`); the shape is
/// generic and accepts any list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateConfig {
    pub expected_roles: Vec<String>,
    pub policy: GatePolicy,
}

/// Per-node walker config: the node's class, the inbound edges that must
/// fire before the node is considered ready, and the gate policy used to
/// fold inbound verdicts. Beta-a precursor — beta-b's walker consumes
/// this; nothing in the FSM reads it yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodeConfig {
    pub class: crate::team::NodeClass,
    pub expected_inbound: Vec<NodeId>,
    pub policy: GatePolicy,
}

/// Adjacency + per-node config for the lifecycle DAG walker. Built from
/// `TeamTopology` by the runtime helper `build_walk_config`. Beta-a
/// precursor — beta-b's walker consumes this; nothing in the FSM
/// reads it yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalkConfig {
    pub adjacency: HashMap<NodeId, Vec<NodeId>>,
    pub node_configs: HashMap<NodeId, NodeConfig>,
}

/// Return value of the pure `decide` function. Transient — not persisted,
/// not serialized, does not appear in `BriefStateRecord`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decide {
    Wait,
    Pass,
    Rework { detail: String },
    Reject { detail: String },
}

/// Pure gate-fan-in decision. Folds the multiset of role-kind → verdict
/// reports collected so far against the gate's policy and expected role
/// list. No I/O, no async, no allocation beyond the returned `Decide`
/// value (the `detail` strings).
///
/// `received` keys are role-kind strings from `lifecycle::role_kind`;
/// values are the verdict that role reported. `gate.expected_roles`
/// enumerates which role-kinds must appear in `received` for the gate
/// to be satisfied.
#[must_use]
pub fn decide(received: &BTreeMap<String, EventVerdict>, gate: &GateConfig) -> Decide {
    match &gate.policy {
        GatePolicy::AllMustPass => decide_all_must_pass(received, &gate.expected_roles),
        GatePolicy::FailFast => decide_fail_fast(received, &gate.expected_roles),
        GatePolicy::Majority { threshold_pct } => {
            decide_majority(received, &gate.expected_roles, *threshold_pct)
        }
    }
}

fn decide_all_must_pass(received: &BTreeMap<String, EventVerdict>, expected: &[String]) -> Decide {
    for (role, verdict) in received {
        if matches!(verdict, EventVerdict::Rejected) {
            return Decide::Reject {
                detail: format!("verifier {role} rejected"),
            };
        }
        if matches!(verdict, EventVerdict::Escalated) {
            return Decide::Reject {
                detail: format!("verifier {role} escalated"),
            };
        }
    }
    for (role, verdict) in received {
        if matches!(verdict, EventVerdict::Failed) {
            return Decide::Rework {
                detail: format!("verifier {role} failed"),
            };
        }
        if matches!(verdict, EventVerdict::ReworkNeeded) {
            return Decide::Rework {
                detail: format!("verifier {role} requested rework"),
            };
        }
    }
    if expected.iter().all(|r| {
        received
            .get(r)
            .is_some_and(|v| matches!(v, EventVerdict::Shipped))
    }) {
        Decide::Pass
    } else {
        Decide::Wait
    }
}

fn decide_fail_fast(received: &BTreeMap<String, EventVerdict>, expected: &[String]) -> Decide {
    for (role, verdict) in received {
        match verdict {
            EventVerdict::Rejected => {
                return Decide::Reject {
                    detail: format!("verifier {role} rejected"),
                };
            }
            EventVerdict::Escalated => {
                return Decide::Reject {
                    detail: format!("verifier {role} escalated"),
                };
            }
            EventVerdict::Failed => {
                return Decide::Rework {
                    detail: format!("verifier {role} failed"),
                };
            }
            EventVerdict::ReworkNeeded => {
                return Decide::Rework {
                    detail: format!("verifier {role} requested rework"),
                };
            }
            EventVerdict::Shipped => {}
        }
    }
    if expected.iter().all(|r| {
        received
            .get(r)
            .is_some_and(|v| matches!(v, EventVerdict::Shipped))
    }) {
        Decide::Pass
    } else {
        Decide::Wait
    }
}

fn decide_majority(
    received: &BTreeMap<String, EventVerdict>,
    expected: &[String],
    threshold_pct: u32,
) -> Decide {
    let n: u32 = u32::try_from(expected.len()).unwrap_or(u32::MAX);
    let mut s: u32 = 0;
    let mut soft_fail: u32 = 0;
    let mut hard_fail: u32 = 0;
    for verdict in received.values() {
        match verdict {
            EventVerdict::Shipped => s += 1,
            EventVerdict::Failed | EventVerdict::ReworkNeeded => soft_fail += 1,
            EventVerdict::Rejected | EventVerdict::Escalated => hard_fail += 1,
        }
    }

    if hard_fail > 0 {
        return Decide::Reject {
            detail: format!("majority gate hard fail count {hard_fail}"),
        };
    }

    let threshold_total = u64::from(threshold_pct) * u64::from(n);
    if u64::from(s) * 100 >= threshold_total {
        return Decide::Pass;
    }

    let received_count: u32 = u32::try_from(received.len()).unwrap_or(u32::MAX);
    let unreported = n.saturating_sub(received_count);
    let max_possible = unreported.saturating_add(s);
    let all_reported = s.saturating_add(soft_fail) == n;

    if all_reported && soft_fail > 0 {
        return Decide::Rework {
            detail: format!("majority threshold {threshold_pct} not reached, soft fails present"),
        };
    }

    if u64::from(max_possible) * 100 < threshold_total {
        return Decide::Reject {
            detail: format!("majority threshold {threshold_pct} unreachable"),
        };
    }

    Decide::Wait
}
