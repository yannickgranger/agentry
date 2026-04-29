//! TeamTopology — the methodology, expressed as data.
//!
//! A team is a set of roles, a message graph between them, and optional
//! permit-override rules that downstream roles inherit from upstream contract
//! messages. The orchestrator runs the graph; the team's composition *is*
//! the methodology.

use crate::role::RoleName;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamName(pub String);

impl fmt::Display for TeamName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A directed edge in the message graph: "from's outbox messages routed to to's inbox".
/// The optional `permit_overrides_from` names a contract-field set that `to` inherits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageEdge {
    pub from: RoleName,
    pub to: RoleName,
    /// If set, when `from` emits a message whose payload contains this key, its
    /// value (a `PermitOverrides`) is applied to `to`'s permit at spawn time.
    /// Example: synthesizer emits `{"permit_overrides": {"fs_write": ["src/a.rs"]}}`;
    /// coder's permit narrows accordingly.
    #[serde(default)]
    pub permit_overrides_from: Option<String>,
}

/// Narrowing constraints that can be inherited from an upstream contract message.
/// Orchestrator validates shape; it does not interpret semantics beyond substitution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamTopology {
    pub name: TeamName,
    pub version: u32,
    /// The roles that compose the team. Each name must resolve to an `AgentRole` record.
    pub roles: Vec<RoleName>,
    /// Directed edges between roles.
    pub message_graph: Vec<MessageEdge>,
    /// The terminal role — when this role emits `done` with a shipped verdict,
    /// the team is considered complete and all containers are torn down.
    pub terminal_role: RoleName,
    /// Max retries on failure before the team verdict becomes `failed`. 0 = no retry.
    #[serde(default)]
    pub max_retries: u32,
}

impl TeamTopology {
    /// Edges whose `from` is the given role — i.e. where its outputs go.
    #[must_use]
    pub fn outgoing(&self, role: &RoleName) -> Vec<&MessageEdge> {
        self.message_graph
            .iter()
            .filter(|e| e.from == *role)
            .collect()
    }

    /// Edges whose `to` is the given role — i.e. where its inputs come from.
    #[must_use]
    pub fn incoming(&self, role: &RoleName) -> Vec<&MessageEdge> {
        self.message_graph
            .iter()
            .filter(|e| e.to == *role)
            .collect()
    }

    /// Distinct upstream role names that feed this role (deduplicated `from`
    /// of `incoming(role)`). Used by the DAG walker to decide when a role's
    /// inbound joins are satisfied.
    #[must_use]
    pub fn inbound_roles(&self, role: &RoleName) -> Vec<&RoleName> {
        let mut out: Vec<&RoleName> = Vec::new();
        for edge in self.incoming(role) {
            if !out.iter().any(|r| **r == edge.from) {
                out.push(&edge.from);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rn(s: &str) -> RoleName {
        RoleName(s.into())
    }

    #[test]
    fn team_roundtrip_json() {
        let t = TeamTopology {
            name: TeamName("qbot-issue-team".into()),
            version: 1,
            roles: vec![
                rn("archaeologist"),
                rn("prescriber"),
                rn("coder-rust"),
                rn("reviewer"),
                rn("shipper"),
            ],
            message_graph: vec![
                MessageEdge {
                    from: rn("archaeologist"),
                    to: rn("prescriber"),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: rn("prescriber"),
                    to: rn("coder-rust"),
                    permit_overrides_from: Some("permit_overrides".into()),
                },
                MessageEdge {
                    from: rn("coder-rust"),
                    to: rn("reviewer"),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: rn("reviewer"),
                    to: rn("shipper"),
                    permit_overrides_from: None,
                },
            ],
            terminal_role: rn("shipper"),
            max_retries: 2,
        };
        let s = serde_json::to_string_pretty(&t).expect("ser");
        let back: TeamTopology = serde_json::from_str(&s).expect("de");
        assert_eq!(t, back);
    }

    #[test]
    fn echo_team_minimal() {
        // The M0 team: one role, one self-edge pointing nowhere (terminal is the only role).
        let t = TeamTopology {
            name: TeamName("echo-team".into()),
            version: 1,
            roles: vec![rn("echo-agent")],
            message_graph: vec![],
            terminal_role: rn("echo-agent"),
            max_retries: 0,
        };
        assert!(t.outgoing(&rn("echo-agent")).is_empty());
        assert!(t.incoming(&rn("echo-agent")).is_empty());
    }

    #[test]
    fn inbound_roles_dedup_and_order() {
        // Two upstreams routing to `to` via two edges from one of them — the
        // helper should deduplicate, preserving first-seen order.
        let t = TeamTopology {
            name: TeamName("t".into()),
            version: 1,
            roles: vec![rn("a"), rn("b"), rn("c")],
            message_graph: vec![
                MessageEdge {
                    from: rn("a"),
                    to: rn("c"),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: rn("b"),
                    to: rn("c"),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: rn("a"),
                    to: rn("c"),
                    permit_overrides_from: Some("k".into()),
                },
            ],
            terminal_role: rn("c"),
            max_retries: 0,
        };
        let upstreams = t.inbound_roles(&rn("c"));
        assert_eq!(upstreams.len(), 2);
        assert_eq!(upstreams[0], &rn("a"));
        assert_eq!(upstreams[1], &rn("b"));
        assert!(t.inbound_roles(&rn("a")).is_empty());
    }

    #[test]
    fn permit_overrides_default_empty() {
        let o = PermitOverrides::default();
        assert!(o.fs_write.is_empty());
        assert!(o.tool_allowlist.is_empty());
    }
}
