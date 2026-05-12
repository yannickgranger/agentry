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

/// The position a brief occupies inside the lifecycle FSM. Non-terminal
/// variants carry their `RetryBudget`; the two terminals (`Shipped`,
/// `Failed`) carry only the outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BriefState {
    Submitted,
    Authoring {
        agent_id: String,
        started_at: Ts,
        retry: RetryBudget,
    },
    Verifying {
        retry: RetryBudget,
        received: BTreeMap<String, EventVerdict>,
        expected: Vec<String>,
        policy: GatePolicy,
    },
    Reviewing {
        retry: RetryBudget,
        received: BTreeMap<String, EventVerdict>,
        expected: Vec<String>,
        policy: GatePolicy,
    },
    Reworking {
        target: ReworkTarget,
        retry: RetryBudget,
    },
    Shipping {
        pr_number: u32,
        head_sha: String,
        retry: RetryBudget,
    },
    Watching {
        pr_number: u32,
        head_sha: String,
        retry: RetryBudget,
    },
    Extension {
        name: String,
        data: serde_json::Value,
        retry: RetryBudget,
    },
    /// Coder reported all-applied=false but every miss carries applied_form+rationale
    /// (deliberate disagreement, not failure). Brief is parked until the captain
    /// accepts or rejects via CaptainAccepted / CaptainRejected events. Carries
    /// the original Authoring retry budget so retry semantics are preserved if
    /// the captain chooses to reject and the operator resubmits.
    AwaitingCaptainDecision {
        disagreements: Vec<DisagreementSummary>,
        retry: RetryBudget,
    },
    /// Beta-a (#495) precursor variant: per-node lifecycle position used
    /// by the DAG walker that beta-b will wire in. Carries the active
    /// node id, the per-node evidence multiset, the variant-specific
    /// run data, and the retry budget. Tagged `walking` to match the
    /// `kind` + snake_case convention of the other variants. Not
    /// reachable from `handle()` in beta-a — `handle()` stubs the arm
    /// with `InvalidTransition` so the compiler stays exhaustive.
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
        #[serde(default = "crate::now")]
        started_at: Ts,
    },
    CoderDone {
        verdict: EventVerdict,
    },
    /// Coder reported terminal Shipped but produced no diff against base
    /// (acceptance passed against work that was already on the base
    /// branch). Short-circuits the FSM Authoring → Shipped, bypassing
    /// the Verifying / Reviewing / Shipping / Watching trail since
    /// there is nothing for downstream roles to act on. The free-text
    /// reason carries the coder's diagnosis for the operator-visible
    /// terminal verdict.
    CoderDoneNoOp {
        reason: String,
    },
    /// Self-review found unapplied verbs but every miss has applied_form+rationale
    /// set. Coder is flagging a deliberate disagreement, not a failure. The FSM
    /// transitions Authoring (or Reworking) to AwaitingCaptainDecision.
    CoderDisagreed {
        disagreements: Vec<DisagreementSummary>,
    },
    AcVerifierDone {
        verdict: EventVerdict,
        role_name: String,
    },
    ReviewerDone {
        verdict: EventVerdict,
        findings: Vec<ReviewFinding>,
        role_name: String,
    },
    ShipperDone {
        pr_number: u32,
        head_sha: String,
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
    /// Captain endorses the disagreed-form output. The FSM treats the brief as
    /// if the coder had shipped normally — proceeds to the post-coder phase
    /// chain (Verifying → Reviewing → Shipping → Watching) using the work
    /// that's already in the brief workspace.
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
    /// Beta-a (#495) precursor variant: per-node done signal that
    /// beta-b's walker consumes. Carries the source node id, the
    /// node's verdict, any review findings, and an optional
    /// `RunData` payload (None for nodes that don't carry per-node
    /// state). Tagged `role_done` to match the `kind` + snake_case
    /// convention. Not consumed by the FSM in beta-a — legacy
    /// CoderDone / AcVerifierDone / ReviewerDone / ShipperDone
    /// variants stay until beta-b.
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
/// Retry-budget contract: when a transition would push `attempt > max` on
/// a non-terminal state, the function returns `Failed{BudgetExhausted}`
/// instead of the proposed next state.
///
/// Time contract: variants whose shape carries a `Ts` (`Authoring.started_at`)
/// are populated with `Ts::default()`; the daemon caller overlays the real
/// wall-clock when wrapping into a `BriefStateRecord`. Keeping `handle`
/// time-free is what makes the transition table testable as a pure table.
///
/// Error type is boxed because [`InvalidTransition`] embeds `BriefState +
/// BriefEvent`, both of which grow whenever a new variant lands; clippy's
/// `result_large_err` lint (denied as error in CI) flags the unboxed
/// shape once the inner pair crosses the 128-byte threshold.
pub fn handle(
    state: &BriefState,
    event: &BriefEvent,
    gates: &PhaseGates,
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
            BriefEvent::CoderDisagreed { disagreements } => {
                // Phase-specific signal: only valid from the coder phase
                // (Authoring or Reworking). The universal handler's
                // "always-applies on non-terminal" semantics don't apply
                // here — from any other state this is an InvalidTransition.
                return match state {
                    BriefState::Authoring { retry, .. } => {
                        Ok(BriefState::AwaitingCaptainDecision {
                            disagreements: disagreements.clone(),
                            retry: *retry,
                        })
                    }
                    BriefState::Reworking { retry, .. } => {
                        Ok(BriefState::AwaitingCaptainDecision {
                            disagreements: disagreements.clone(),
                            retry: *retry,
                        })
                    }
                    _ => Err(Box::new(InvalidTransition {
                        from: state.clone(),
                        event: event.clone(),
                    })),
                };
            }
            _ => {}
        }
    }

    match (state, event) {
        // ---- Submitted ----
        (
            BriefState::Submitted,
            BriefEvent::CoderStarted {
                agent_id,
                started_at,
            },
        ) => Ok(BriefState::Authoring {
            agent_id: agent_id.clone(),
            started_at: *started_at,
            retry: RetryBudget {
                attempt: 1,
                max: DEFAULT_ATTEMPT_CAP,
            },
        }),

        // ---- Authoring ----
        // Preflight smell: preflight-criterion-agentry detected an
        // operator-authored criterion that triggers one of the smell
        // heuristics. Per Q1/Q3 of the brief 84b grill-me transcript,
        // smell is a terminal block (no warn-and-continue, no
        // operator-override): the criterion itself is the contract and
        // refining the heuristics is a code-level PR. Routes through
        // Authoring because preflight currently has no state of its
        // own; 84b-2 will revisit the FSM if a dedicated `Preflight`
        // state turns out to be worth the variant.
        (BriefState::Authoring { .. }, BriefEvent::PreflightSmellDetected { .. }) => {
            Ok(BriefState::Failed {
                reason: Reason::PreflightSmell,
            })
        }

        // No-op short-circuit: acceptance passed against work that was
        // already on the base branch. Skip Verifying / Reviewing /
        // Shipping / Watching — there is no diff for downstream roles
        // to operate on. The lifecycle driver overrides the terminal
        // verdict's reason with the carried free-text so the operator
        // sees "no-op brief — ..." on `agentry:verdicts`.
        (BriefState::Authoring { .. }, BriefEvent::CoderDoneNoOp { .. }) => Ok(BriefState::Shipped),

        (BriefState::Authoring { retry, .. }, BriefEvent::CoderDone { verdict }) => match verdict {
            EventVerdict::Shipped => Ok(post_coder_shipped(*retry, gates)),
            EventVerdict::Failed => Ok(BriefState::Failed {
                reason: Reason::AcceptanceFailed {
                    detail: "coder reported failed".to_owned(),
                },
            }),
            EventVerdict::Escalated => Ok(BriefState::Failed {
                reason: Reason::AcceptanceFailed {
                    detail: "coder escalated".to_owned(),
                },
            }),
            EventVerdict::Rejected => Ok(BriefState::Failed {
                reason: Reason::AcceptanceFailed {
                    detail: "coder rejected".to_owned(),
                },
            }),
            EventVerdict::ReworkNeeded => invalid(),
        },

        // ---- Verifying ----
        (
            BriefState::Verifying {
                retry,
                received,
                expected,
                policy,
            },
            BriefEvent::AcVerifierDone { verdict, role_name },
        ) => {
            let mut new_received = received.clone();
            new_received.insert(role_name.clone(), *verdict);
            let gate_config = GateConfig {
                expected_roles: expected.clone(),
                policy: policy.clone(),
            };
            match decide(&new_received, &gate_config) {
                Decide::Wait => Ok(BriefState::Verifying {
                    retry: *retry,
                    received: new_received,
                    expected: expected.clone(),
                    policy: policy.clone(),
                }),
                Decide::Pass => {
                    // Empty-phase auto-skip: if reviewing has no expected
                    // roles, decide() Passes vacuously and we short-circuit
                    // straight to Shipping rather than stalling in Reviewing.
                    let received_r = BTreeMap::new();
                    let gate_r = GateConfig {
                        expected_roles: gates.reviewing.expected_roles.clone(),
                        policy: gates.reviewing.policy.clone(),
                    };
                    match decide(&received_r, &gate_r) {
                        Decide::Pass => Ok(BriefState::Shipping {
                            pr_number: 0,
                            head_sha: String::new(),
                            retry: *retry,
                        }),
                        _ => Ok(BriefState::Reviewing {
                            retry: *retry,
                            received: received_r,
                            expected: gate_r.expected_roles,
                            policy: gate_r.policy,
                        }),
                    }
                }
                Decide::Rework { detail: _ } => {
                    Ok(increment_or_fail(*retry, |next| BriefState::Reworking {
                        target: ReworkTarget::Coder,
                        retry: next,
                    }))
                }
                Decide::Reject { detail } => Ok(BriefState::Failed {
                    reason: Reason::AcceptanceFailed { detail },
                }),
            }
        }

        // ---- Reviewing ----
        (
            BriefState::Reviewing {
                retry,
                received,
                expected,
                policy,
            },
            BriefEvent::ReviewerDone {
                verdict,
                findings: _,
                role_name,
            },
        ) => {
            let mut new_received = received.clone();
            new_received.insert(role_name.clone(), *verdict);
            let gate_config = GateConfig {
                expected_roles: expected.clone(),
                policy: policy.clone(),
            };
            match decide(&new_received, &gate_config) {
                Decide::Wait => Ok(BriefState::Reviewing {
                    retry: *retry,
                    received: new_received,
                    expected: expected.clone(),
                    policy: policy.clone(),
                }),
                Decide::Pass => Ok(BriefState::Shipping {
                    pr_number: 0,
                    head_sha: String::new(),
                    retry: *retry,
                }),
                Decide::Rework { detail: _ } => {
                    Ok(increment_or_fail(*retry, |next| BriefState::Reworking {
                        target: ReworkTarget::Coder,
                        retry: next,
                    }))
                }
                Decide::Reject { detail } => Ok(BriefState::Failed {
                    reason: Reason::AcceptanceFailed { detail },
                }),
            }
        }

        // ---- Reworking ----
        (
            BriefState::Reworking { retry, .. },
            BriefEvent::CoderStarted {
                agent_id,
                started_at,
            },
        ) => Ok(BriefState::Authoring {
            agent_id: agent_id.clone(),
            started_at: *started_at,
            retry: *retry,
        }),

        // ---- AwaitingCaptainDecision ----
        // Captain endorses the disagreed-form output: treat it as if the
        // coder had emitted CoderDone{Shipped} from Authoring. Use the
        // shared post_coder_shipped helper so the empty-phase auto-skip
        // and gate-evaluation logic stays in one place.
        (BriefState::AwaitingCaptainDecision { retry, .. }, BriefEvent::CaptainAccepted) => {
            Ok(post_coder_shipped(*retry, gates))
        }
        (BriefState::AwaitingCaptainDecision { .. }, BriefEvent::CaptainRejected { reason }) => {
            Ok(BriefState::Failed {
                reason: Reason::CaptainRejectedDisagreement {
                    reason: reason.clone(),
                },
            })
        }

        // ---- Shipping ----
        (
            BriefState::Shipping { retry, .. },
            BriefEvent::ShipperDone {
                pr_number,
                head_sha,
            },
        ) => Ok(BriefState::Watching {
            pr_number: *pr_number,
            head_sha: head_sha.clone(),
            retry: *retry,
        }),

        // ---- Watching ----
        (
            BriefState::Watching {
                pr_number,
                head_sha,
                retry,
            },
            event,
        ) => match event {
            BriefEvent::CiResult { state: ci, .. } => match ci {
                CiState::Success => Ok(BriefState::Shipped),
                CiState::Failed => Ok(increment_or_fail(*retry, |next| BriefState::Reworking {
                    target: ReworkTarget::Coder,
                    retry: next,
                })),
                CiState::Pending => Ok(BriefState::Watching {
                    pr_number: *pr_number,
                    head_sha: head_sha.clone(),
                    retry: *retry,
                }),
            },
            BriefEvent::RebaseStarted => Ok(BriefState::Watching {
                pr_number: *pr_number,
                head_sha: head_sha.clone(),
                retry: *retry,
            }),
            BriefEvent::Rebased { new_head_sha } => Ok(BriefState::Watching {
                pr_number: *pr_number,
                head_sha: new_head_sha.clone(),
                retry: *retry,
            }),
            _ => invalid(),
        },

        // ---- Failed (terminal except for human-driven retry) ----
        (BriefState::Failed { .. }, BriefEvent::RetryRequested { .. }) => Ok(BriefState::Submitted),

        // ---- Walking (beta-a precursor — not yet wired) ----
        // Beta-a lands the variant alongside the legacy phase enum so
        // beta-b's walker can replace the phase chain in place. The
        // FSM does not yet route any event into Walking, but match
        // exhaustivity requires an explicit arm here once the variant
        // exists. Reject every event landing on a Walking state with
        // InvalidTransition; beta-b rewrites this with the real
        // walker semantics.
        (BriefState::Walking { .. }, _) => invalid(),

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

/// Post-coder-Shipped phase routing: evaluate verifying then reviewing
/// gates against an empty received multiset, applying the empty-phase
/// auto-skip rule (decide() returns Pass vacuously when expected_roles
/// is empty). Shared by the `Authoring + CoderDone{Shipped}` arm and the
/// `AwaitingCaptainDecision + CaptainAccepted` arm so both call sites
/// stay byte-equivalent.
fn post_coder_shipped(retry: RetryBudget, gates: &PhaseGates) -> BriefState {
    let received_v = BTreeMap::new();
    let gate_v = GateConfig {
        expected_roles: gates.verifying.expected_roles.clone(),
        policy: gates.verifying.policy.clone(),
    };
    match decide(&received_v, &gate_v) {
        Decide::Pass => {
            let received_r = BTreeMap::new();
            let gate_r = GateConfig {
                expected_roles: gates.reviewing.expected_roles.clone(),
                policy: gates.reviewing.policy.clone(),
            };
            match decide(&received_r, &gate_r) {
                Decide::Pass => BriefState::Shipping {
                    pr_number: 0,
                    head_sha: String::new(),
                    retry,
                },
                _ => BriefState::Reviewing {
                    retry,
                    received: received_r,
                    expected: gate_r.expected_roles,
                    policy: gate_r.policy,
                },
            }
        }
        _ => BriefState::Verifying {
            retry,
            received: received_v,
            expected: gate_v.expected_roles,
            policy: gate_v.policy,
        },
    }
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

/// Per-brief container for the verifying-phase and reviewing-phase
/// `GateConfig` values. 396b will populate this from team topology at
/// brief dispatch time and thread it through `handle()`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGates {
    pub verifying: GateConfig,
    pub reviewing: GateConfig,
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
