//! ValidatorPipeline — daemon-facing classification of a brief by execution
//! policy, plus the `From<TaskShape>` boundary translation.
//!
//! See `specs/concepts/brief_classification.md` for the full council
//! decision artifact and the rationale behind the rename.

use crate::kind::TaskShape;
use serde::{Deserialize, Serialize};

/// Daemon-facing classification of a brief by execution policy.
///
/// Each variant names a chain of validators dispatched by
/// `crates/validators::registry_for`. Wire format is snake_case for
/// backward compatibility with pre-rename serialized briefs; renamed
/// variants carry serde aliases to their previous wire forms.
///
/// **Homonym warning:** `Mechanical` exists in both `TaskShape` and
/// `ValidatorPipeline` with different semantics. See
/// `specs/concepts/brief_classification.md` invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorPipeline {
    Refactor,
    #[serde(alias = "debug")]
    BugFix,
    Mechanical,
    #[serde(alias = "new_feature")]
    Feature,
    Substrate,
    #[serde(alias = "audit")]
    Triage,
    #[serde(alias = "doc")]
    TrivialDoc,
}

impl From<TaskShape> for ValidatorPipeline {
    fn from(shape: TaskShape) -> Self {
        match shape {
            TaskShape::TrivialDoc => ValidatorPipeline::TrivialDoc,
            TaskShape::TrivialMechanical => ValidatorPipeline::Mechanical,
            TaskShape::Mechanical => ValidatorPipeline::Mechanical,
            TaskShape::BugFix => ValidatorPipeline::BugFix,
            TaskShape::Feature => ValidatorPipeline::Feature,
            // TODO(captain-doctrine): Migration may warrant a purpose-built
            // pipeline once the InMemory→real-infra migration topology is built.
            TaskShape::Migration => ValidatorPipeline::Feature,
            // TODO(captain-doctrine): Portage maps to Refactor pending a
            // dedicated portage pipeline that adds cfdb clones-invariant checks.
            TaskShape::Portage => ValidatorPipeline::Refactor,
            // TODO(captain-doctrine): Sweep maps to Refactor pending a
            // dedicated sweep pipeline that defers compile to integration.
            TaskShape::Sweep => ValidatorPipeline::Refactor,
            TaskShape::Triage => ValidatorPipeline::Triage,
        }
    }
}
