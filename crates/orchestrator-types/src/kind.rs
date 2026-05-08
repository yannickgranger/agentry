//! BriefKind — typed classification of a brief by task shape.
//!
//! Drives the contract-required predicate at intake (later slices) and seeds
//! the vocabulary the topology catalog draws from. Pure types + predicate;
//! Brief integration is B3, daemon validation is B6.

use serde::{Deserialize, Serialize};

/// Task-shape classification of a brief. Authored by the brief submitter;
/// future slices key contract requirements and topology selection off this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BriefKind {
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

impl BriefKind {
    /// Whether a brief of this kind requires a typed Contract at intake.
    ///
    /// Match is exhaustive over all variants on purpose: adding a future
    /// variant must force a deliberate contract-requirement decision; a
    /// wildcard arm would let new variants silently default.
    #[must_use]
    pub fn requires_contract(self) -> bool {
        match self {
            BriefKind::TrivialDoc | BriefKind::TrivialMechanical => false,
            BriefKind::Mechanical
            | BriefKind::BugFix
            | BriefKind::Feature
            | BriefKind::Migration
            | BriefKind::Portage
            | BriefKind::Sweep
            | BriefKind::Triage => true,
        }
    }
}
