//! TeamTopology — the methodology, expressed as data.
//!
//! A team is a set of roles, a message graph between them, and optional
//! permit-override rules that downstream roles inherit from upstream contract
//! messages. The orchestrator runs the graph; the team's composition *is*
//! the methodology.

use crate::role::RoleRef;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TeamName(pub String);

impl fmt::Display for TeamName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A directed edge in the message graph: "from's outbox messages routed to to's inbox".
/// The optional `permit_overrides_from` names a contract-field set that `to` inherits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageEdge {
    pub from: RoleRef,
    pub to: RoleRef,
    /// If set, when `from` emits a message whose payload contains this key, its
    /// value (a `PermitOverrides`) is applied to `to`'s permit at spawn time.
    /// Example: synthesizer emits `{"permit_overrides": {"fs_write": ["src/a.rs"]}}`;
    /// coder's permit narrows accordingly.
    #[serde(default)]
    pub permit_overrides_from: Option<String>,
    /// When this edge's `to` role emits a `ReworkNeeded` verdict, route the
    /// rework back to this role instead of the immediate upstream (`from`).
    /// Default: `None` (daemon falls back to single-upstream rework — the
    /// current behavior). Used in self-host workflows to make the coder the
    /// rework target for code-level rejections at any downstream role.
    /// Brief 191b makes the daemon actually consume this; 191a only ships
    /// the vocabulary and validation.
    #[serde(default)]
    pub rework_target: Option<RoleRef>,
}

/// Narrowing constraints that can be inherited from an upstream contract message.
/// Orchestrator validates shape; it does not interpret semantics beyond substitution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermitOverrides {
    /// Narrow `fs:write:*` scopes to only these paths.
    #[serde(default)]
    pub fs_write: Vec<String>,
    /// Narrow `fs:read:*` scopes to only these paths.
    #[serde(default)]
    pub fs_read: Vec<String>,
    /// Narrow the tool allowlist to this intersection.
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
    /// Arbitrary additional constraints — interpreted by roles, not orchestrator.
    #[serde(default, flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// The team topology.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TeamTopology {
    pub name: TeamName,
    pub version: u32,
    /// The roles that compose the team. Each `RoleRef` must resolve to an
    /// `AgentRole` record at the pinned `(name, version)`.
    pub roles: Vec<RoleRef>,
    /// Directed edges between roles.
    pub message_graph: Vec<MessageEdge>,
    /// The terminal role — when this role emits `done` with a shipped verdict,
    /// the team is considered complete and all containers are torn down.
    pub terminal_role: RoleRef,
    /// Max retries on failure before the team verdict becomes `failed`. 0 = no retry.
    #[serde(default)]
    pub max_retries: u32,
}

impl TeamTopology {
    /// Edges whose `from` is the given role — i.e. where its outputs go.
    #[must_use]
    pub fn outgoing(&self, role: &RoleRef) -> Vec<&MessageEdge> {
        self.message_graph
            .iter()
            .filter(|e| e.from == *role)
            .collect()
    }

    /// Edges whose `to` is the given role — i.e. where its inputs come from.
    #[must_use]
    pub fn incoming(&self, role: &RoleRef) -> Vec<&MessageEdge> {
        self.message_graph
            .iter()
            .filter(|e| e.to == *role)
            .collect()
    }

    /// Distinct upstream role refs that feed this role (deduplicated `from`
    /// of `incoming(role)`). Used by the DAG walker to decide when a role's
    /// inbound joins are satisfied.
    #[must_use]
    pub fn inbound_roles(&self, role: &RoleRef) -> Vec<&RoleRef> {
        let mut out: Vec<&RoleRef> = Vec::new();
        for edge in self.incoming(role) {
            if !out.iter().any(|r| **r == edge.from) {
                out.push(&edge.from);
            }
        }
        out
    }
}
