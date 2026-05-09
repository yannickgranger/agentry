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

/// Redeploy targets a brief may require after merge. The captain CLI's
/// `redeploy` subcommand (F8b) reads this field and runs the appropriate
/// rebuild for each listed target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedeployTarget {
    Daemon,
    OrchestratorCli,
    CaptainCli,
}

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
    /// Captain-authored task shape. Optional for backwards compatibility;
    /// existing payloads deserialize to `None`. The daemon translates this
    /// into a `crate::pipeline::ValidatorPipeline` via the `From` impl in
    /// `pipeline.rs` before dispatching validators.
    #[serde(default)]
    pub kind: Option<crate::kind::TaskShape>,
    /// Optional validation contract authored by the captain. When
    /// `kind.requires_contract()` is true and `contract` is None, the daemon
    /// logs a WARN at intake (B3) and will reject at intake in a later slice
    /// (B6). Existing briefs without this field deserialize with
    /// `contract: None`.
    #[serde(default)]
    pub contract: Option<crate::contract::Contract>,
    /// Hard budget; runtime enforces.
    #[serde(default)]
    pub budget: Budget,
    /// How to handle decisions outside standing orders.
    #[serde(default)]
    pub escalation: EscalationMode,
    /// If this brief replaces/extends an earlier one, reference it.
    #[serde(default)]
    pub parent_brief: Option<BriefId>,
    /// Free-form cohort labels propagated to every agent the brief spawns.
    /// Set by the dispatching authority (captain/officer/human submitter);
    /// the orchestrator does not assign or interpret them. Monitoring
    /// selectors use these to address subsets of the agent fleet.
    #[serde(default)]
    pub cohort_labels: Vec<String>,
    /// Targets that must be redeployed after this brief merges. F8b's
    /// captain `redeploy` subcommand reads this and runs the rebuild;
    /// F8a only carries the data. Empty (and skipped on the wire) for
    /// briefs that don't touch redeployable code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redeploy_required: Vec<RedeployTarget>,
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
            kind: None,
            contract: None,
            budget: Budget::default(),
            escalation: EscalationMode::default(),
            parent_brief: None,
            cohort_labels: Vec::new(),
            redeploy_required: Vec::new(),
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

    #[must_use]
    pub fn with_cohort_labels(mut self, labels: Vec<String>) -> Self {
        self.cohort_labels = labels;
        self
    }
}
