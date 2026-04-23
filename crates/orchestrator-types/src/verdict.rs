//! Verdict — the terminal record for a completed brief.
//!
//! Appended to `agentry:verdicts` stream. Drives the dashboard's verdict-history
//! view and satisfies the "no verdict, no close" drift rule.

use crate::{Ts, brief::BriefId, event::Verdict as EventVerdict, now};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Shipped,
    Failed,
    Escalated,
    PermitViolation,
    BudgetExceeded,
    Aborted,
}

impl From<EventVerdict> for VerdictKind {
    fn from(v: EventVerdict) -> Self {
        match v {
            EventVerdict::Shipped => Self::Shipped,
            EventVerdict::Failed => Self::Failed,
            EventVerdict::Escalated => Self::Escalated,
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
        }
    }

    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_roundtrip_json() {
        let v = Verdict::new(BriefId("brf_xyz".into()), VerdictKind::Shipped)
            .with_reason("echo completed");
        let s = serde_json::to_string(&v).expect("ser");
        let back: Verdict = serde_json::from_str(&s).expect("de");
        assert_eq!(v, back);
        assert!(v.trace_stream.contains("brf_xyz"));
    }

    #[test]
    fn event_verdict_maps() {
        assert_eq!(VerdictKind::from(EventVerdict::Shipped), VerdictKind::Shipped);
        assert_eq!(VerdictKind::from(EventVerdict::Failed), VerdictKind::Failed);
    }
}
