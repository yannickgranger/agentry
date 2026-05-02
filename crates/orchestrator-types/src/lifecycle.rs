//! Brief lifecycle state machine — pure types and transition function.
//!
//! Implements the FSM described in `specs/concepts/brief_lifecycle.md` and
//! the retry budget mechanics in `specs/concepts/brief_retry_budget.md`.
//!
//! `handle` is a pure function of `(state, event)`. Wall-clock time and
//! brief-id wrapping are layered by the daemon caller (see L.2).

use crate::{BriefId, EventVerdict, ReviewFinding, Ts};
use serde::{Deserialize, Serialize};

/// Compile-time default for `RetryBudget.max` when a topology does not
/// specify `max_retries`.
pub const DEFAULT_ATTEMPT_CAP: u32 = 3;

/// Compile-time hard ceiling. Topologies declaring `max_retries` above
/// this are rejected at dispatch with `Reason::AcceptanceFailed`.
pub const MAXIMUM_ATTEMPT_CAP: u32 = 10;

/// Persisted projection of a brief's current lifecycle position. The daemon
/// writes one of these per FSM step; the projector replays them on resume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    AbortRequested { actor: String, message: String },
    AcceptanceFailed { detail: String },
    DaemonError { detail: String },
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

/// The position a brief occupies inside the lifecycle FSM. Non-terminal
/// variants carry their `RetryBudget`; the two terminals (`Shipped`,
/// `Failed`) carry only the outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    },
    Reviewing {
        retry: RetryBudget,
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
    },
    CoderDone {
        verdict: EventVerdict,
    },
    AcVerifierDone {
        verdict: EventVerdict,
    },
    ReviewerDone {
        verdict: EventVerdict,
        findings: Vec<ReviewFinding>,
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
    BudgetExhausted,
}

/// Returned when an event is not legal in the current state. Carries an
/// owned snapshot of both so the caller can log or surface the bad pair
/// without re-borrowing.
#[derive(Debug, Clone, PartialEq)]
pub struct InvalidTransition {
    pub from: BriefState,
    pub event: BriefEvent,
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
pub fn handle(state: &BriefState, event: &BriefEvent) -> Result<BriefState, InvalidTransition> {
    let invalid = || {
        Err(InvalidTransition {
            from: state.clone(),
            event: event.clone(),
        })
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
        (BriefState::Submitted, BriefEvent::CoderStarted { agent_id }) => {
            Ok(BriefState::Authoring {
                agent_id: agent_id.clone(),
                started_at: Ts::default(),
                retry: RetryBudget {
                    attempt: 1,
                    max: DEFAULT_ATTEMPT_CAP,
                },
            })
        }

        // ---- Authoring ----
        (BriefState::Authoring { retry, .. }, BriefEvent::CoderDone { verdict }) => match verdict {
            EventVerdict::Shipped => Ok(BriefState::Verifying { retry: *retry }),
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
        (BriefState::Verifying { retry }, BriefEvent::AcVerifierDone { verdict }) => {
            match verdict {
                EventVerdict::Shipped => Ok(BriefState::Reviewing { retry: *retry }),
                EventVerdict::ReworkNeeded | EventVerdict::Failed => {
                    Ok(increment_or_fail(*retry, |next| BriefState::Reworking {
                        target: ReworkTarget::Coder,
                        retry: next,
                    }))
                }
                EventVerdict::Rejected => Ok(BriefState::Failed {
                    reason: Reason::AcceptanceFailed {
                        detail: "ac-verifier rejected".to_owned(),
                    },
                }),
                EventVerdict::Escalated => Ok(BriefState::Failed {
                    reason: Reason::AcceptanceFailed {
                        detail: "ac-verifier escalated".to_owned(),
                    },
                }),
            }
        }

        // ---- Reviewing ----
        (
            BriefState::Reviewing { retry },
            BriefEvent::ReviewerDone {
                verdict,
                findings: _,
            },
        ) => match verdict {
            EventVerdict::Shipped => Ok(BriefState::Shipping {
                pr_number: 0,
                head_sha: String::new(),
                retry: *retry,
            }),
            EventVerdict::ReworkNeeded => {
                Ok(increment_or_fail(*retry, |next| BriefState::Reworking {
                    target: ReworkTarget::Coder,
                    retry: next,
                }))
            }
            EventVerdict::Failed | EventVerdict::Rejected => Ok(BriefState::Failed {
                reason: Reason::AcceptanceFailed {
                    detail: "reviewer rejected".to_owned(),
                },
            }),
            EventVerdict::Escalated => Ok(BriefState::Failed {
                reason: Reason::AcceptanceFailed {
                    detail: "reviewer escalated".to_owned(),
                },
            }),
        },

        // ---- Reworking ----
        (BriefState::Reworking { retry, .. }, BriefEvent::CoderStarted { agent_id }) => {
            Ok(BriefState::Authoring {
                agent_id: agent_id.clone(),
                started_at: Ts::default(),
                retry: *retry,
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
