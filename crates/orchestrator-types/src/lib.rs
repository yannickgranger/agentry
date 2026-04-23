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
pub use role::{AgentRole, Mount, PackageManager, RoleName, SubstrateClass};
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

#[cfg(test)]
mod apply_overrides_tests {
    use super::*;

    fn sample_permit() -> WorkPermit {
        WorkPermit {
            permit_id: "x".into(),
            agent_id: "x".into(),
            role: RoleName("x".into()),
            brief: brief::BriefId("x".into()),
            tool_allowlist: ToolAllowlist(vec!["read".into(), "write".into(), "edit".into()]),
            permit_scope: PermitScope(vec![
                "fs:read:/workspace/**".into(),
                "fs:write:/workspace/**".into(),
            ]),
            max_tokens: None,
            max_wall_seconds: None,
            max_usd: None,
            expires_at: now(),
            issued_at: now(),
            signature: None,
        }
    }

    #[test]
    fn fs_write_narrowing_replaces_wildcards() {
        let mut p = sample_permit();
        let o = PermitOverrides {
            fs_write: vec!["/workspace/a.rs".into()],
            ..Default::default()
        };
        apply_overrides(&mut p, &o);
        assert!(!p
            .permit_scope
            .0
            .iter()
            .any(|s| s == "fs:write:/workspace/**"));
        assert!(p
            .permit_scope
            .0
            .contains(&"fs:write:/workspace/a.rs".into()));
        // fs:read untouched.
        assert!(p.permit_scope.0.contains(&"fs:read:/workspace/**".into()));
    }

    #[test]
    fn allowlist_override_intersects() {
        let mut p = sample_permit();
        let o = PermitOverrides {
            tool_allowlist: vec!["read".into(), "write".into()],
            ..Default::default()
        };
        apply_overrides(&mut p, &o);
        assert!(p.tool_allowlist.contains("read"));
        assert!(p.tool_allowlist.contains("write"));
        assert!(!p.tool_allowlist.contains("edit")); // dropped — not in override
    }

    #[test]
    fn check_tool_call_allows_in_scope_write() {
        let mut p = sample_permit();
        apply_overrides(
            &mut p,
            &PermitOverrides {
                fs_write: vec!["/workspace/a.rs".into()],
                ..Default::default()
            },
        );
        let ok = check_tool_call(&p, "write", &serde_json::json!({"path":"/workspace/a.rs"}));
        assert!(ok.is_ok());
        let nope = check_tool_call(&p, "write", &serde_json::json!({"path":"/workspace/b.rs"}));
        assert!(nope.is_err());
    }

    #[test]
    fn check_tool_call_blocks_unknown_tool() {
        let p = sample_permit();
        let nope = check_tool_call(&p, "shell", &serde_json::json!({}));
        assert!(nope.is_err());
    }
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
