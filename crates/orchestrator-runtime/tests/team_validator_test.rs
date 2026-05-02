//! Public-surface tests for the team-topology validator.

use orchestrator_runtime::team_validator::{validate, TeamValidationViolation, ViolationKind};
use orchestrator_types::{MessageEdge, RoleName, RoleRef, TeamName, TeamTopology};
use std::collections::HashSet;

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
                rework_target: None,
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
fn accepts_rework_target_in_roles() {
    // a→b, with edge.rework_target = a (resolves in roles[]) → no violations.
    let mut t = topo(
        "t",
        &[("a", 1), ("b", 1)],
        &[(("a", 1), ("b", 1))],
        ("b", 1),
        1,
    );
    t.message_graph[0].rework_target = Some(rr("a", 1));
    let v = validate(&t, &registry(&[("a", 1), ("b", 1)]));
    assert!(v.is_empty(), "expected zero violations, got: {v:?}");
}

#[test]
fn accepts_no_rework_target() {
    // Regression: existing workflows have rework_target=None and must validate cleanly.
    let t = topo(
        "t",
        &[("a", 1), ("b", 1)],
        &[(("a", 1), ("b", 1))],
        ("b", 1),
        1,
    );
    assert!(t.message_graph[0].rework_target.is_none());
    let v = validate(&t, &registry(&[("a", 1), ("b", 1)]));
    assert!(v.is_empty(), "expected zero violations, got: {v:?}");
}

#[test]
fn rejects_rework_target_not_in_roles() {
    // a→b with rework_target = ghost (NOT in roles[]) → exactly one Reference violation
    // on message_graph[0].rework_target.
    let mut t = topo(
        "t",
        &[("a", 1), ("b", 1)],
        &[(("a", 1), ("b", 1))],
        ("b", 1),
        1,
    );
    t.message_graph[0].rework_target = Some(rr("ghost", 1));
    let v = validate(&t, &registry(&[("a", 1), ("b", 1)]));
    let rework_violations: Vec<&TeamValidationViolation> = v
        .iter()
        .filter(|x| {
            x.kind == ViolationKind::Reference && x.path == "message_graph[0].rework_target"
        })
        .collect();
    assert_eq!(
        rework_violations.len(),
        1,
        "expected exactly one Reference violation on message_graph[0].rework_target, got: {v:?}"
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
