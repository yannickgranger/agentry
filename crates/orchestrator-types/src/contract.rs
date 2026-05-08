//! Contract — typed validation contract authored by the captain per brief.
//!
//! A contract declares what must be true after a brief lands. Each
//! `Assertion` is anchored either to a cfdb qname (verifiable structurally),
//! a graph-specs concept (verifiable as spec-conformance), or a behavior
//! target (verifiable against real infra). Assertions without an anchor are
//! rejected at parse time — the discriminator is mandatory by the enum's
//! shape, and `deny_unknown_fields` forbids stray top-level fields.
//!
//! This module is the SCHEMA only. Brief integration (B3), captain CLI
//! (B5), and daemon validation (B6) are downstream slices.

use crate::brief::BriefId;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Identifier for a single assertion within a contract (e.g. `"A1"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssertionId(pub String);

impl fmt::Display for AssertionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an assertion is grounded. Discriminated union: every assertion is
/// anchored to exactly one verifiable form. The `kind` discriminator is
/// emitted on the wire as `"cfdb"`, `"spec_concept"`, or `"behavior"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AssertionAnchor {
    /// A cfdb qname that must resolve in the workspace's cfdb keyspace.
    Cfdb { qname: String },
    /// A section in `specs/concepts/` that must resolve via graph-specs.
    SpecConcept { path: PathBuf, section: String },
    /// A live-system target verifiable against real infra.
    Behavior { live_target: String },
}

/// One claim about post-brief state. Carries an id, a prose description,
/// and an anchor that grounds the claim in something verifiable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    pub id: AssertionId,
    pub prose: String,
    pub anchor: AssertionAnchor,
}

/// Top-level container. A signed brief carries an optional Contract; this
/// brief introduces the type only — Brief integration is a later slice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub brief_id: BriefId,
    pub assertions: Vec<Assertion>,
    pub precursor_artifacts: Vec<PathBuf>,
}
