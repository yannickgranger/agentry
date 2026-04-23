//! Pure types for the agentry orchestrator.
//!
//! Four data records (Brief, AgentRole, TeamTopology, Project) plus the runtime
//! bookkeeping types (WorkPermit, Verdict, Event). No logic here — only shapes
//! and serde round-trip.

#![forbid(unsafe_code)]

pub mod brief;
pub mod event;
pub mod permit;
pub mod project;
pub mod role;
pub mod team;
pub mod verdict;

pub use brief::{Brief, BriefId, Budget, EscalationMode, Payload};
pub use event::{Event, EventKind, ToolCall, Verdict as EventVerdict};
pub use permit::{PermitScope, ToolAllowlist, WorkPermit};
pub use project::{Project, ProjectSlug, StandingOrders};
pub use role::{AgentRole, RoleName, SubstrateClass};
pub use team::{MessageEdge, PermitOverrides, TeamName, TeamTopology};
pub use verdict::{Verdict, VerdictKind};

use serde::{Deserialize, Serialize};

/// Redis key namespace — everything lives under `agentry:`.
pub const NS: &str = "agentry";

/// Typed error for shape validation.
#[derive(Debug, thiserror::Error)]
pub enum TypeError {
    #[error("invalid id: {0}")]
    InvalidId(String),
    #[error("invalid reference: {0}")]
    InvalidRef(String),
    #[error("schema violation: {0}")]
    Schema(String),
}

/// Monotonic timestamp for events and verdicts.
pub type Ts = chrono::DateTime<chrono::Utc>;

/// Produce a fresh UTC timestamp.
#[must_use]
pub fn now() -> Ts {
    chrono::Utc::now()
}

/// A versioned reference to a Role or Team record.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionedRef {
    pub name: String,
    pub version: u32,
}

impl VersionedRef {
    #[must_use]
    pub fn new(name: impl Into<String>, version: u32) -> Self {
        Self {
            name: name.into(),
            version,
        }
    }

    #[must_use]
    pub fn redis_key(&self, kind: &str) -> String {
        format!("{NS}:{kind}:{}:v{}", self.name, self.version)
    }
}
