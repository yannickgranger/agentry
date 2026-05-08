//! Pure types for the agentry orchestrator.
//!
//! Data records (Brief, AgentRole, TeamTopology, Project, Contract), task-shape
//! and validator descriptors (TaskShape, ValidatorPipeline), plus the runtime
//! bookkeeping types (WorkPermit, Verdict, Event). No logic here — only shapes
//! and serde round-trip.

#![forbid(unsafe_code)]

pub mod brief;
pub mod contract;
pub mod event;
pub mod kind;
pub mod lifecycle;
pub mod permit;
pub mod pipeline;
pub mod profile;
pub mod project;
pub mod review;
pub mod role;
pub mod team;
pub mod verdict;

pub use brief::{Brief, BriefId, Budget, EscalationMode, Payload};
pub use contract::{Assertion, AssertionAnchor, AssertionId, Contract};
pub use event::{DoneReason, Event, EventKind, EventVerdict, ToolCall};
pub use kind::TaskShape;
pub use permit::{PermitScope, ToolAllowlist, WorkPermit};
pub use pipeline::ValidatorPipeline;
pub use profile::{
    parse_profile_toml, Profile, ProfileAcceptanceSection, ProfileMethodologySection,
    ProfileParseError, ProfileRoleSection,
};
pub use project::{Project, ProjectSlug, StandingOrders};
pub use review::{FindingOrigin, ReviewFinding, Severity};
pub use role::{
    merge_role_with_packs, AgentRole, AllowedTools, Mount, PackageManager, RoleName, RoleRef,
    SubstrateClass, ToolPack, WorkspaceMount,
};
pub use team::{MessageEdge, PermitOverrides, TeamName, TeamTopology};

/// Apply a `PermitOverrides` payload to an already-minted permit, in place.
///
/// Orthogonal narrowing:
/// - `fs_write` non-empty → drop all existing `fs:write:*` scope entries and
///   add `fs:write:<p>` for each listed path (intersection becomes the list).
/// - `fs_read` the same.
/// - `tool_allowlist` non-empty → intersect the permit's allowlist with the
///   override list (permit keeps only tools present in BOTH).
///
/// Empty fields are no-ops (baseline scope preserved).
pub fn apply_overrides(permit: &mut WorkPermit, o: &PermitOverrides) {
    if !o.fs_write.is_empty() {
        permit
            .permit_scope
            .0
            .retain(|s| !s.starts_with("fs:write:"));
        for p in &o.fs_write {
            permit.permit_scope.0.push(format!("fs:write:{p}"));
        }
    }
    if !o.fs_read.is_empty() {
        permit.permit_scope.0.retain(|s| !s.starts_with("fs:read:"));
        for p in &o.fs_read {
            permit.permit_scope.0.push(format!("fs:read:{p}"));
        }
    }
    if !o.tool_allowlist.is_empty() {
        use std::collections::HashSet;
        let want: HashSet<&String> = o.tool_allowlist.iter().collect();
        permit.tool_allowlist.0.retain(|t| want.contains(t));
    }
}

/// Check if a tool call is permitted. Returns Ok(()) on allow; Err(reason) on block.
/// Rules:
///   1. `tool` must be in `permit.tool_allowlist`.
///   2. If `tool` is filesystem-write-ish (`write`, `edit`) and `args.path` is
///      a string, it must match one of the permit's `fs:write:*` scope entries.
///      Match is literal-suffix for M6; glob matching is a later milestone.
pub fn check_tool_call(
    permit: &WorkPermit,
    tool: &str,
    args: &serde_json::Value,
) -> Result<(), String> {
    if !permit.allows(tool) {
        return Err(format!("unauthorized tool call: {tool}"));
    }
    const WRITE_TOOLS: &[&str] = &["write", "edit"];
    if WRITE_TOOLS.contains(&tool) {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            let allowed = permit
                .permit_scope
                .0
                .iter()
                .any(|s| s.strip_prefix("fs:write:").is_some_and(|p| p == path));
            if !allowed {
                return Err(format!("fs:write scope denied: {path}"));
            }
        }
    }
    Ok(())
}

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
