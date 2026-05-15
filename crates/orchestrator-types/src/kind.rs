//! TaskShape — typed classification of a brief by authoring intent.
//!
//! Captain-facing enum: the brief author declares which task shape they
//! scoped. Drives the contract-required predicate at intake (later slices)
//! and seeds the vocabulary the topology catalog draws from. The daemon
//! consumes a derived `ValidatorPipeline` produced by
//! `From<TaskShape> for ValidatorPipeline`; see `pipeline.rs`.
//!
//! Renamed from `BriefKind` by B2.5 of the captain-doctrine work to
//! resolve a name collision with the validator-pipeline enum. File path
//! preserved for git-history continuity from B2.

use serde::{Deserialize, Serialize};

/// Task-shape classification of a brief. Authored by the brief submitter;
/// future slices key contract requirements and topology selection off this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskShape {
    TrivialDoc,
    TrivialMechanical,
    Mechanical,
    BugFix,
    Feature,
    Migration,
    Portage,
    Sweep,
    Triage,
}

impl TaskShape {
    /// Whether a brief of this kind requires a typed Contract at intake.
    ///
    /// Match is exhaustive over all variants on purpose: adding a future
    /// variant must force a deliberate contract-requirement decision; a
    /// wildcard arm would let new variants silently default.
    #[must_use]
    pub fn requires_contract(self) -> bool {
        match self {
            TaskShape::TrivialDoc | TaskShape::TrivialMechanical => false,
            TaskShape::Mechanical
            | TaskShape::BugFix
            | TaskShape::Feature
            | TaskShape::Migration
            | TaskShape::Portage
            | TaskShape::Sweep
            | TaskShape::Triage => true,
        }
    }
}
