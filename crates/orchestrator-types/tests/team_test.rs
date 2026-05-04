use orchestrator_types::{MessageEdge, PermitOverrides, RoleName, RoleRef, TeamName, TeamTopology};

fn rr(s: &str) -> RoleRef {
    RoleRef {
        name: RoleName(s.into()),
        version: 1,
    }
}

#[test]
fn team_roundtrip_json() {
    let t = TeamTopology {
        name: TeamName("qbot-issue-team".into()),
        version: 1,
        roles: vec![
            rr("archaeologist"),
            rr("prescriber"),
            rr("coder-rust"),
            rr("reviewer"),
            rr("shipper"),
        ],
        message_graph: vec![
            MessageEdge {
                from: rr("archaeologist"),
                to: rr("prescriber"),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: rr("prescriber"),
                to: rr("coder-rust"),
                permit_overrides_from: Some("permit_overrides".into()),
                rework_target: None,
            },
            MessageEdge {
                from: rr("coder-rust"),
                to: rr("reviewer"),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: rr("reviewer"),
                to: rr("shipper"),
                permit_overrides_from: None,
                rework_target: None,
            },
        ],
        terminal_role: rr("shipper"),
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
        roles: vec![rr("echo-agent")],
        message_graph: vec![],
        terminal_role: rr("echo-agent"),
        max_retries: 0,
    };
    assert!(t.outgoing(&rr("echo-agent")).is_empty());
    assert!(t.incoming(&rr("echo-agent")).is_empty());
}

#[test]
fn inbound_roles_dedup_and_order() {
    // Two upstreams routing to `to` via two edges from one of them — the
    // helper should deduplicate, preserving first-seen order.
    let t = TeamTopology {
        name: TeamName("t".into()),
        version: 1,
        roles: vec![rr("a"), rr("b"), rr("c")],
        message_graph: vec![
            MessageEdge {
                from: rr("a"),
                to: rr("c"),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: rr("b"),
                to: rr("c"),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: rr("a"),
                to: rr("c"),
                permit_overrides_from: Some("k".into()),
                rework_target: None,
            },
        ],
        terminal_role: rr("c"),
        max_retries: 0,
    };
    let upstreams = t.inbound_roles(&rr("c"));
    assert_eq!(upstreams.len(), 2);
    assert_eq!(upstreams[0], &rr("a"));
    assert_eq!(upstreams[1], &rr("b"));
    assert!(t.inbound_roles(&rr("a")).is_empty());
}

#[test]
fn permit_overrides_default_empty() {
    let o = PermitOverrides::default();
    assert!(o.fs_write.is_empty());
    assert!(o.tool_allowlist.is_empty());
}
