//! Pure-logic validator for [`TeamTopology`].
//!
//! Runs five independent checks against a topology and a list of registered
//! roles, returning the union of every violation found (no short-circuit).
//! Has no I/O, no Redis access, no async — callers pass borrowed inputs and
//! receive an owned vector back.
//!
//! Vocabulary integrity (rejecting unknown fields at parse time) is provided
//! structurally by `#[serde(deny_unknown_fields)]` on the workflow types; it
//! is not a runtime check here.

use orchestrator_types::{RoleName, RoleRef, TeamTopology};
use std::collections::{HashMap, HashSet};

/// One violation surfaced by [`validate`]. The `path` names the offending
/// field (e.g. `"roles[2]"`, `"message_graph[0].from"`, `"terminal_role"`),
/// `kind` classifies it, and `detail` is a human-readable description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamValidationViolation {
    pub path: String,
    pub kind: ViolationKind,
    pub detail: String,
}

/// Categories of failures the validator surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ViolationKind {
    /// Type-integrity failure (empty/zero where a non-empty/non-zero value is required).
    Type,
    /// Reference-integrity failure (a name does not resolve in its target set).
    Reference,
    /// Topological failure (no entry, unreachable terminal, orphaned role).
    Topological,
    /// `message_graph` contains a cycle.
    Acyclic,
    /// Multiple roles have no outbound edges, or the unique sink is not `terminal_role`.
    MultipleTerminals,
}

/// Run every check and return the union of violations. Does not short-circuit.
#[must_use]
pub fn validate(
    topology: &TeamTopology,
    registered_roles: &[(RoleName, u32)],
) -> Vec<TeamValidationViolation> {
    let mut out = Vec::new();
    check_type_integrity(topology, &mut out);
    check_reference_integrity(topology, registered_roles, &mut out);
    check_topological_integrity(topology, &mut out);
    check_acyclic(topology, &mut out);
    check_single_terminal(topology, &mut out);
    out
}

fn check_type_integrity(topology: &TeamTopology, out: &mut Vec<TeamValidationViolation>) {
    if topology.version == 0 {
        out.push(TeamValidationViolation {
            path: "version".into(),
            kind: ViolationKind::Type,
            detail: "version must be > 0".into(),
        });
    }
    if topology.name.0.is_empty() {
        out.push(TeamValidationViolation {
            path: "name".into(),
            kind: ViolationKind::Type,
            detail: "name must be non-empty".into(),
        });
    }
    if topology.roles.is_empty() {
        out.push(TeamValidationViolation {
            path: "roles".into(),
            kind: ViolationKind::Type,
            detail: "roles[] must be non-empty".into(),
        });
    }
    if topology.terminal_role.name.0.is_empty() {
        out.push(TeamValidationViolation {
            path: "terminal_role".into(),
            kind: ViolationKind::Type,
            detail: "terminal_role must be non-empty".into(),
        });
    }
}

fn check_reference_integrity(
    topology: &TeamTopology,
    registered: &[(RoleName, u32)],
    out: &mut Vec<TeamValidationViolation>,
) {
    let registered_set: HashSet<(&RoleName, u32)> =
        registered.iter().map(|(n, v)| (n, *v)).collect();
    for (i, role) in topology.roles.iter().enumerate() {
        if !registered_set.contains(&(&role.name, role.version)) {
            out.push(TeamValidationViolation {
                path: format!("roles[{i}]"),
                kind: ViolationKind::Reference,
                detail: format!(
                    "role '{}' v{} is not in registered_roles",
                    role.name.0, role.version
                ),
            });
        }
    }
    let role_set: HashSet<&RoleRef> = topology.roles.iter().collect();
    for (i, edge) in topology.message_graph.iter().enumerate() {
        if !role_set.contains(&edge.from) {
            out.push(TeamValidationViolation {
                path: format!("message_graph[{i}].from"),
                kind: ViolationKind::Reference,
                detail: format!(
                    "from '{}' v{} is not in topology.roles",
                    edge.from.name.0, edge.from.version
                ),
            });
        }
        if !role_set.contains(&edge.to) {
            out.push(TeamValidationViolation {
                path: format!("message_graph[{i}].to"),
                kind: ViolationKind::Reference,
                detail: format!(
                    "to '{}' v{} is not in topology.roles",
                    edge.to.name.0, edge.to.version
                ),
            });
        }
    }
    if !role_set.contains(&topology.terminal_role) {
        out.push(TeamValidationViolation {
            path: "terminal_role".into(),
            kind: ViolationKind::Reference,
            detail: format!(
                "terminal_role '{}' v{} is not in topology.roles",
                topology.terminal_role.name.0, topology.terminal_role.version
            ),
        });
    }
}

fn build_outbound_adjacency(topology: &TeamTopology) -> HashMap<&RoleRef, Vec<&RoleRef>> {
    let role_set: HashSet<&RoleRef> = topology.roles.iter().collect();
    let mut adj: HashMap<&RoleRef, Vec<&RoleRef>> = HashMap::new();
    for edge in &topology.message_graph {
        if role_set.contains(&edge.from) && role_set.contains(&edge.to) {
            adj.entry(&edge.from).or_default().push(&edge.to);
        }
    }
    adj
}

fn entry_roles(topology: &TeamTopology) -> Vec<&RoleRef> {
    let role_set: HashSet<&RoleRef> = topology.roles.iter().collect();
    let mut has_inbound: HashSet<&RoleRef> = HashSet::new();
    for edge in &topology.message_graph {
        if role_set.contains(&edge.to) {
            has_inbound.insert(&edge.to);
        }
    }
    topology
        .roles
        .iter()
        .filter(|r| !has_inbound.contains(r))
        .collect()
}

fn reachable_from<'a>(
    seeds: &[&'a RoleRef],
    adj: &HashMap<&'a RoleRef, Vec<&'a RoleRef>>,
) -> HashSet<&'a RoleRef> {
    let mut reachable: HashSet<&'a RoleRef> = HashSet::new();
    let mut stack: Vec<&'a RoleRef> = seeds.to_vec();
    while let Some(node) = stack.pop() {
        if reachable.insert(node) {
            if let Some(neighbors) = adj.get(node) {
                for n in neighbors {
                    stack.push(n);
                }
            }
        }
    }
    reachable
}

fn check_topological_integrity(topology: &TeamTopology, out: &mut Vec<TeamValidationViolation>) {
    if topology.roles.is_empty() {
        return;
    }
    let entries = entry_roles(topology);
    if entries.is_empty() {
        out.push(TeamValidationViolation {
            path: "message_graph".into(),
            kind: ViolationKind::Topological,
            detail: "no entry role: every role has an inbound edge".into(),
        });
        return;
    }
    let adj = build_outbound_adjacency(topology);
    let reachable = reachable_from(&entries, &adj);

    let role_set: HashSet<&RoleRef> = topology.roles.iter().collect();
    if role_set.contains(&topology.terminal_role) && !reachable.contains(&topology.terminal_role) {
        out.push(TeamValidationViolation {
            path: "terminal_role".into(),
            kind: ViolationKind::Topological,
            detail: format!(
                "terminal_role '{}' v{} is not reachable from any entry",
                topology.terminal_role.name.0, topology.terminal_role.version
            ),
        });
    }
    flag_orphans(topology, out);
}

fn flag_orphans(topology: &TeamTopology, out: &mut Vec<TeamValidationViolation>) {
    // A role is orphaned when it never appears as `from` or `to` of any edge
    // AND the topology has more than one role (single-role topologies like
    // echo-team are intentionally edgeless).
    if topology.roles.len() <= 1 {
        return;
    }
    let role_set: HashSet<&RoleRef> = topology.roles.iter().collect();
    let mut referenced: HashSet<&RoleRef> = HashSet::new();
    for edge in &topology.message_graph {
        if role_set.contains(&edge.from) {
            referenced.insert(&edge.from);
        }
        if role_set.contains(&edge.to) {
            referenced.insert(&edge.to);
        }
    }
    for (i, role) in topology.roles.iter().enumerate() {
        if !referenced.contains(role) {
            out.push(TeamValidationViolation {
                path: format!("roles[{i}]"),
                kind: ViolationKind::Topological,
                detail: format!(
                    "role '{}' v{} is orphaned: never referenced by any edge",
                    role.name.0, role.version
                ),
            });
        }
    }
}

fn dfs_has_cycle<'a>(
    node: &'a RoleRef,
    adj: &HashMap<&'a RoleRef, Vec<&'a RoleRef>>,
    visited: &mut HashSet<&'a RoleRef>,
    on_stack: &mut HashSet<&'a RoleRef>,
) -> bool {
    if on_stack.contains(node) {
        return true;
    }
    if !visited.insert(node) {
        return false;
    }
    on_stack.insert(node);
    if let Some(neighbors) = adj.get(node) {
        for n in neighbors {
            if dfs_has_cycle(n, adj, visited, on_stack) {
                return true;
            }
        }
    }
    on_stack.remove(node);
    false
}

fn check_acyclic(topology: &TeamTopology, out: &mut Vec<TeamValidationViolation>) {
    let adj = build_outbound_adjacency(topology);
    let mut visited: HashSet<&RoleRef> = HashSet::new();
    let mut on_stack: HashSet<&RoleRef> = HashSet::new();
    for role in &topology.roles {
        if dfs_has_cycle(role, &adj, &mut visited, &mut on_stack) {
            out.push(TeamValidationViolation {
                path: "message_graph".into(),
                kind: ViolationKind::Acyclic,
                detail: "message_graph contains a cycle".into(),
            });
            return;
        }
    }
}

fn check_single_terminal(topology: &TeamTopology, out: &mut Vec<TeamValidationViolation>) {
    if topology.roles.is_empty() {
        return;
    }
    let role_set: HashSet<&RoleRef> = topology.roles.iter().collect();
    let mut has_outbound: HashSet<&RoleRef> = HashSet::new();
    for edge in &topology.message_graph {
        if role_set.contains(&edge.from) {
            has_outbound.insert(&edge.from);
        }
    }
    let sinks: Vec<&RoleRef> = topology
        .roles
        .iter()
        .filter(|r| !has_outbound.contains(r))
        .collect();
    if sinks.len() > 1 {
        let names: Vec<String> = sinks
            .iter()
            .map(|r| format!("{} v{}", r.name.0, r.version))
            .collect();
        out.push(TeamValidationViolation {
            path: "message_graph".into(),
            kind: ViolationKind::MultipleTerminals,
            detail: format!("multiple roles have no outbound edges: {names:?}"),
        });
    } else if sinks.len() == 1 && sinks[0] != &topology.terminal_role {
        out.push(TeamValidationViolation {
            path: "terminal_role".into(),
            kind: ViolationKind::MultipleTerminals,
            detail: format!(
                "the unique sink role '{}' v{} is not the declared terminal_role '{}' v{}",
                sinks[0].name.0,
                sinks[0].version,
                topology.terminal_role.name.0,
                topology.terminal_role.version
            ),
        });
    }
    // sinks.is_empty() implies a cycle covering every role, already reported by acyclic check.
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_types::{MessageEdge, TeamName, TeamTopology};

    type RoleSpec<'a> = (&'a str, u32);
    type EdgeSpec<'a> = (RoleSpec<'a>, RoleSpec<'a>);

    fn rr(s: &str, v: u32) -> RoleRef {
        RoleRef {
            name: RoleName(s.into()),
            version: v,
        }
    }

    fn topo(
        name: &str,
        roles: &[RoleSpec<'_>],
        edges: &[EdgeSpec<'_>],
        terminal: RoleSpec<'_>,
        version: u32,
    ) -> TeamTopology {
        TeamTopology {
            name: TeamName(name.into()),
            version,
            roles: roles.iter().map(|(s, v)| rr(s, *v)).collect(),
            message_graph: edges
                .iter()
                .map(|((f, fv), (t, tv))| MessageEdge {
                    from: rr(f, *fv),
                    to: rr(t, *tv),
                    permit_overrides_from: None,
                })
                .collect(),
            terminal_role: rr(terminal.0, terminal.1),
            max_retries: 0,
        }
    }

    fn registry(roles: &[(&str, u32)]) -> Vec<(RoleName, u32)> {
        roles
            .iter()
            .map(|(s, v)| (RoleName((*s).into()), *v))
            .collect()
    }

    #[test]
    fn validates_self_host_v1_clean() {
        // Same shape as seed.rs::agentry_self_host_v1: 4 roles, linear pipeline,
        // ci-watcher terminal.
        let t = topo(
            "agentry-self-host-v1",
            &[
                ("coder-claude-agentry", 1),
                ("reviewer-claude-agentry", 1),
                ("git-operator", 1),
                ("ci-watcher-agentry", 1),
            ],
            &[
                (("coder-claude-agentry", 1), ("reviewer-claude-agentry", 1)),
                (("reviewer-claude-agentry", 1), ("git-operator", 1)),
                (("git-operator", 1), ("ci-watcher-agentry", 1)),
            ],
            ("ci-watcher-agentry", 1),
            1,
        );
        let reg = registry(&[
            ("coder-claude-agentry", 1),
            ("reviewer-claude-agentry", 1),
            ("git-operator", 1),
            ("ci-watcher-agentry", 1),
        ]);
        let v = validate(&t, &reg);
        assert!(v.is_empty(), "expected zero violations, got: {v:?}");
    }

    #[test]
    fn rejects_unknown_field_in_messageedge() {
        let json = r#"{"from":{"name":"a","version":1},"to":{"name":"b","version":1},"bogus":1}"#;
        let r: Result<MessageEdge, _> = serde_json::from_str(json);
        assert!(r.is_err(), "expected unknown-field rejection, got: {r:?}");
    }

    #[test]
    fn rejects_unknown_field_in_teamtopology() {
        let json = r#"{
            "name":"t",
            "version":1,
            "roles":[{"name":"a","version":1}],
            "message_graph":[],
            "terminal_role":{"name":"a","version":1},
            "extra_top":42
        }"#;
        let r: Result<TeamTopology, _> = serde_json::from_str(json);
        assert!(r.is_err(), "expected unknown-field rejection, got: {r:?}");
    }

    #[test]
    fn detects_zero_version() {
        let t = topo("t", &[("a", 1)], &[], ("a", 1), 0);
        let v = validate(&t, &registry(&[("a", 1)]));
        assert!(
            v.iter()
                .any(|x| x.kind == ViolationKind::Type && x.path == "version"),
            "expected Type violation on version, got: {v:?}"
        );
    }

    #[test]
    fn detects_empty_name() {
        let t = topo("", &[("a", 1)], &[], ("a", 1), 1);
        let v = validate(&t, &registry(&[("a", 1)]));
        assert!(
            v.iter()
                .any(|x| x.kind == ViolationKind::Type && x.path == "name"),
            "expected Type violation on name, got: {v:?}"
        );
    }

    #[test]
    fn detects_unregistered_role() {
        let t = topo("t", &[("missing-role", 1)], &[], ("missing-role", 1), 1);
        let v = validate(&t, &[]);
        assert!(
            v.iter()
                .any(|x| x.kind == ViolationKind::Reference && x.path.starts_with("roles[")),
            "expected Reference violation on roles[i], got: {v:?}"
        );
    }

    #[test]
    fn rejects_unregistered_version() {
        // Register only X v1; the topology references X v2 — must surface a
        // Reference violation. Versioned references mean a known name with an
        // unknown version is just as invalid as a missing name.
        let t = topo("t", &[("x", 2)], &[], ("x", 2), 1);
        let v = validate(&t, &registry(&[("x", 1)]));
        assert!(
            v.iter()
                .any(|x| x.kind == ViolationKind::Reference && x.path.starts_with("roles[")),
            "expected Reference violation for x v2 (only v1 registered), got: {v:?}"
        );
    }

    #[test]
    fn accepts_distinct_versions() {
        // Both X v1 AND X v2 are registered; the topology references both as
        // independent roles. Distinct (name, version) pairs must validate
        // cleanly — no Reference violations and no orphans.
        let t = topo(
            "t",
            &[("x", 1), ("x", 2)],
            &[(("x", 1), ("x", 2))],
            ("x", 2),
            1,
        );
        let v = validate(&t, &registry(&[("x", 1), ("x", 2)]));
        assert!(v.is_empty(), "expected zero violations, got: {v:?}");
    }

    #[test]
    fn detects_edge_to_unlisted_role() {
        // Edge from=A to=B, but only A is in roles[].
        let t = topo("t", &[("a", 1)], &[(("a", 1), ("b", 1))], ("a", 1), 1);
        let v = validate(&t, &registry(&[("a", 1), ("b", 1)]));
        assert!(
            v.iter()
                .any(|x| x.kind == ViolationKind::Reference && x.path == "message_graph[0].to"),
            "expected Reference violation on edge.to, got: {v:?}"
        );
    }

    #[test]
    fn detects_no_entry() {
        // Every role has an inbound edge: a→b, b→a (also a cycle, but we
        // assert the Topological "no entry" violation specifically).
        let t = topo(
            "t",
            &[("a", 1), ("b", 1)],
            &[(("a", 1), ("b", 1)), (("b", 1), ("a", 1))],
            ("b", 1),
            1,
        );
        let v = validate(&t, &registry(&[("a", 1), ("b", 1)]));
        assert!(
            v.iter()
                .any(|x| x.kind == ViolationKind::Topological && x.detail.contains("no entry")),
            "expected Topological no-entry violation, got: {v:?}"
        );
    }

    #[test]
    fn detects_unreachable_terminal() {
        // a→b is the only chain reachable from the lone entry `a`. c↔d is a
        // disconnected cycle whose nodes have inbound, so they are not entries
        // and are unreachable from any entry — terminal `d` is therefore
        // unreachable. (This setup also fires Acyclic and MultipleTerminals
        // violations, but the test asserts only on the terminal-unreachable
        // Topological violation.)
        let t = topo(
            "t",
            &[("a", 1), ("b", 1), ("c", 1), ("d", 1)],
            &[
                (("a", 1), ("b", 1)),
                (("c", 1), ("d", 1)),
                (("d", 1), ("c", 1)),
            ],
            ("d", 1),
            1,
        );
        let v = validate(&t, &registry(&[("a", 1), ("b", 1), ("c", 1), ("d", 1)]));
        assert!(
            v.iter().any(|x| x.kind == ViolationKind::Topological
                && x.path == "terminal_role"
                && x.detail.contains("not reachable")),
            "expected Topological terminal-unreachable violation, got: {v:?}"
        );
    }

    #[test]
    fn detects_orphaned_role() {
        // a→b is the live pipeline (b terminal); c is in roles[] but never referenced.
        let t = topo(
            "t",
            &[("a", 1), ("b", 1), ("c", 1)],
            &[(("a", 1), ("b", 1))],
            ("b", 1),
            1,
        );
        let v = validate(&t, &registry(&[("a", 1), ("b", 1), ("c", 1)]));
        assert!(
            v.iter()
                .any(|x| x.kind == ViolationKind::Topological && x.detail.contains("orphaned")),
            "expected Topological orphaned violation, got: {v:?}"
        );
    }

    #[test]
    fn detects_cycle() {
        // a→b, b→c, c→a — cycle. Also no entry; we assert specifically on Acyclic.
        let t = topo(
            "t",
            &[("a", 1), ("b", 1), ("c", 1)],
            &[
                (("a", 1), ("b", 1)),
                (("b", 1), ("c", 1)),
                (("c", 1), ("a", 1)),
            ],
            ("c", 1),
            1,
        );
        let v = validate(&t, &registry(&[("a", 1), ("b", 1), ("c", 1)]));
        assert!(
            v.iter().any(|x| x.kind == ViolationKind::Acyclic),
            "expected Acyclic violation, got: {v:?}"
        );
    }

    #[test]
    fn detects_multiple_terminals() {
        // a→b and a→c: both b and c have no outbound. Terminal=b.
        let t = topo(
            "t",
            &[("a", 1), ("b", 1), ("c", 1)],
            &[(("a", 1), ("b", 1)), (("a", 1), ("c", 1))],
            ("b", 1),
            1,
        );
        let v = validate(&t, &registry(&[("a", 1), ("b", 1), ("c", 1)]));
        assert!(
            v.iter().any(|x| x.kind == ViolationKind::MultipleTerminals),
            "expected MultipleTerminals violation, got: {v:?}"
        );
    }

    #[test]
    fn collects_multiple_violations() {
        // version=0 (Type) AND unregistered role (Reference) AND empty name (Type).
        let t = topo("", &[("nope", 1)], &[], ("nope", 1), 0);
        let v = validate(&t, &[]);
        let kinds: HashSet<ViolationKind> = v.iter().map(|x| x.kind).collect();
        assert!(
            kinds.contains(&ViolationKind::Type) && kinds.contains(&ViolationKind::Reference),
            "expected both Type and Reference violations, got kinds={kinds:?} all={v:?}"
        );
        // At least 3 distinct violations: zero version, empty name, unregistered role.
        assert!(
            v.len() >= 3,
            "expected ≥3 violations (no short-circuit), got {}: {v:?}",
            v.len()
        );
    }
}
