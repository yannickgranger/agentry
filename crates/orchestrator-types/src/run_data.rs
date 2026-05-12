//! Per-node run-data carried through the lifecycle DAG walker.
//!
//! Beta-a (#495 split part 1) lands the type alongside the legacy
//! `BriefState` shape; beta-b wires it into the FSM. The variant is
//! tagged `kind` + snake_case so JSON renderings match the existing
//! lifecycle vocabulary.

use crate::lifecycle::DisagreementSummary;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Per-node run-data carried through the lifecycle DAG walker. Eq is
/// intentionally not derived: `Extension { data: serde_json::Value }`
/// blocks it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunData {
    None,
    Coder {
        agent_id: String,
    },
    PrTracking {
        pr_number: u32,
        head_sha: String,
    },
    OperatorDecision {
        disagreements: Vec<DisagreementSummary>,
    },
    Extension {
        data: serde_json::Value,
    },
}

impl RunData {
    #[must_use]
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Coder { agent_id } => Some(agent_id.as_str()),
            _ => None,
        }
    }

    #[must_use]
    pub fn pr_number(&self) -> Option<u32> {
        match self {
            Self::PrTracking { pr_number, .. } => Some(*pr_number),
            _ => None,
        }
    }

    #[must_use]
    pub fn head_sha(&self) -> Option<&str> {
        match self {
            Self::PrTracking { head_sha, .. } => Some(head_sha.as_str()),
            _ => None,
        }
    }

    #[must_use]
    pub fn disagreements(&self) -> Option<&[DisagreementSummary]> {
        match self {
            Self::OperatorDecision { disagreements } => Some(disagreements.as_slice()),
            _ => None,
        }
    }
}
