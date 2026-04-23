//! Brief — the unit of work.
//!
//! Submitted on the `agentry:briefs` Redis stream. Immutable after submission.
//! Scope changes = a new Brief with `parent_brief` set.

use crate::{now, Ts, VersionedRef};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Brief identifier: `brf_<uuidv7>`. Sortable by creation time.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BriefId(pub String);

impl BriefId {
    /// Generate a fresh brief id using UUIDv7 (time-ordered).
    #[must_use]
    pub fn fresh() -> Self {
        Self(format!("brf_{}", uuid::Uuid::now_v7()))
    }
}

impl fmt::Display for BriefId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Escalation mode: how the brief handles decisions outside standing orders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationMode {
    /// No human involvement; team decides based on standing orders.
    Autonomous,
    /// Human ack required at phase-end transitions. Default — safest.
    #[default]
    Supervised,
    /// Human decides every step.
    Manual,
}

/// Hard budget caps for a brief. Permit broker enforces.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub max_tokens: Option<u64>,
    pub max_wall_seconds: Option<u64>,
    pub max_usd: Option<f64>,
}

/// Freeform payload: what the team is asked to do. Typed at the team level,
/// opaque to the orchestrator.
pub type Payload = serde_json::Value;

/// A Brief — the unit of work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    /// Unique id.
    pub id: BriefId,
    /// Project slug this brief belongs to (optional for ad-hoc briefs).
    pub project: Option<String>,
    /// Which team topology should handle this brief.
    pub topology: VersionedRef,
    /// Opaque payload — the team interprets it.
    pub payload: Payload,
    /// Hard budget; runtime enforces.
    #[serde(default)]
    pub budget: Budget,
    /// How to handle decisions outside standing orders.
    #[serde(default)]
    pub escalation: EscalationMode,
    /// If this brief replaces/extends an earlier one, reference it.
    #[serde(default)]
    pub parent_brief: Option<BriefId>,
    /// Who submitted this brief (opaque identifier of the client).
    pub submitted_by: String,
    /// Submission time.
    pub submitted_at: Ts,
}

impl Brief {
    /// Build a new brief with a fresh id and current timestamp.
    #[must_use]
    pub fn new(submitted_by: impl Into<String>, topology: VersionedRef, payload: Payload) -> Self {
        Self {
            id: BriefId::fresh(),
            project: None,
            topology,
            payload,
            budget: Budget::default(),
            escalation: EscalationMode::default(),
            parent_brief: None,
            submitted_by: submitted_by.into(),
            submitted_at: now(),
        }
    }

    #[must_use]
    pub fn with_project(mut self, slug: impl Into<String>) -> Self {
        self.project = Some(slug.into());
        self
    }

    #[must_use]
    pub fn with_budget(mut self, b: Budget) -> Self {
        self.budget = b;
        self
    }

    #[must_use]
    pub fn with_escalation(mut self, m: EscalationMode) -> Self {
        self.escalation = m;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn brief_roundtrip_json() {
        let b = Brief::new(
            "user@example.com",
            VersionedRef::new("echo-team", 1),
            json!({"kind": "echo", "msg": "hello"}),
        )
        .with_project("qbot-core")
        .with_escalation(EscalationMode::Autonomous);
        let s = serde_json::to_string(&b).expect("serialize");
        let back: Brief = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(b, back);
        assert!(s.contains("brf_"), "brief id should have prefix");
    }

    #[test]
    fn brief_id_is_prefixed() {
        let id = BriefId::fresh();
        assert!(id.0.starts_with("brf_"));
    }

    #[test]
    fn default_escalation_is_supervised() {
        assert_eq!(EscalationMode::default(), EscalationMode::Supervised);
    }
}
