//! Verdict — the terminal record for a completed brief.
//!
//! Appended to `agentry:verdicts` stream. Drives the dashboard's verdict-history
//! view and satisfies the "no verdict, no close" drift rule.

use crate::{brief::BriefId, event::EventVerdict, now, review::ReviewFinding, Ts};
use serde::{Deserialize, Serialize};

/// The terminal kind of a role's outcome.
///
/// `ReworkNeeded` carries findings so the daemon can route them back to the
/// upstream worker via the team's `message_graph`. `Rejected` is the
/// "unfixable — don't bother retrying" escape hatch; it short-circuits the
/// rework loop and produces a `Failed` team verdict.
///
/// Not `Copy` — `ReworkNeeded` carries a `Vec`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Shipped,
    Failed,
    Escalated,
    PermitViolation,
    BudgetExceeded,
    Aborted,
    Rejected,
    ReworkNeeded { findings: Vec<ReviewFinding> },
}

impl From<EventVerdict> for VerdictKind {
    fn from(v: EventVerdict) -> Self {
        match v {
            EventVerdict::Shipped => Self::Shipped,
            EventVerdict::Failed => Self::Failed,
            EventVerdict::Escalated => Self::Escalated,
            EventVerdict::Rejected => Self::Rejected,
            // Findings travel as separate events and are merged by the
            // spawner's `compute_verdict` — this placeholder lets callers
            // with no accumulated findings still produce a valid kind.
            EventVerdict::ReworkNeeded => Self::ReworkNeeded { findings: vec![] },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub brief: BriefId,
    pub kind: VerdictKind,
    pub at: Ts,
    /// Pointer to the brief's trace stream (for dashboard linking).
    pub trace_stream: String,
    /// Optional short reason for the verdict.
    pub reason: Option<String>,
    /// Number of refusals observed across the brief's role runs.
    #[serde(default)]
    pub refusal_count: u32,
}

impl Verdict {
    #[must_use]
    pub fn new(brief: BriefId, kind: VerdictKind) -> Self {
        let trace_stream = format!("agentry:brief:{}:trace", brief.0);
        Self {
            brief,
            kind,
            at: now(),
            trace_stream,
            reason: None,
            refusal_count: 0,
        }
    }

    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}
