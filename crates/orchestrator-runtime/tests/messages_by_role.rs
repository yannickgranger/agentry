//! #539 phase-5 fence — `daemon::group_messages_by_role` must
//! reconstruct the exact `RoutedMessage` set the in-process
//! `all_messages` accumulator + `.filter(|m| m.to == role).cloned()`
//! would produce. Pinning this equivalence is the precondition for
//! deleting `all_messages` in phase 5b/5c: without a deterministic
//! proof, the swap to trace-derivation is unverifiable (a clean
//! no-rework brief routes zero messages, so a live canary proves
//! nothing about the message path — see the 5a PR body).

use chrono::{TimeZone, Utc};
use orchestrator_runtime::daemon::{group_messages_by_role, overrides_from_messages};
use orchestrator_runtime::spawner::RoutedMessage;
use orchestrator_types::team::{MessageEdge, TeamTopology};
use orchestrator_types::{Event, EventKind, EventVerdict, RoleName, RoleRef, TeamName};
use serde_json::json;

fn ts(secs: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0)
        .single()
        .expect("valid fixed ts")
}

fn spawned(role: &str) -> Event {
    Event {
        at: ts(0),
        kind: EventKind::Event {
            payload: json!({ "agent_event": "spawned", "role_name": role }),
        },
    }
}

fn message(to: &str, body: serde_json::Value, at_secs: i64) -> Event {
    Event {
        at: ts(at_secs),
        kind: EventKind::Message {
            to: to.to_string(),
            payload: body,
        },
    }
}

/// The one message shape that actually flows in a real brief:
/// reviewer rewinds to coder with findings. A clean brief routes zero
/// messages, so this rework path is the case the deletion must not
/// break. The reconstructed `from` is the source role's name, matching
/// the in-process write at spawner.rs:478-484
/// (`RoutedMessage.from = role.name.0`).
#[test]
fn rework_findings_message_reconstructed_with_source_role_from() {
    let entries = vec![
        ("agt_coder".to_string(), spawned("coder-claude-agentry")),
        ("agt_rev".to_string(), spawned("reviewer-claude-agentry")),
        (
            "agt_rev".to_string(),
            message(
                "coder-claude-agentry",
                json!({ "findings": [{ "severity": "blocker" }] }),
                5,
            ),
        ),
    ];
    let out = group_messages_by_role(&entries);
    let inbox = out.get("coder-claude-agentry").expect("coder has an inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].from, "reviewer-claude-agentry");
    assert_eq!(inbox[0].to, "coder-claude-agentry");
    assert_eq!(
        inbox[0].payload,
        json!({ "findings": [{ "severity": "blocker" }] })
    );
    assert_eq!(inbox[0].at, ts(5));
    assert_eq!(out.len(), 1, "no spurious inboxes");
}

/// In-process semantics: `all_messages` is a Vec in arrival order;
/// per-role inbox = `.filter(|m| m.to == role)`. Reconstruction must
/// produce the same grouping AND the same per-role arrival order.
#[test]
fn equivalence_with_in_process_filter_semantics() {
    let entries = vec![
        ("a1".to_string(), spawned("coder-claude-agentry")),
        ("a2".to_string(), spawned("reviewer-mechanical-agentry")),
        ("a3".to_string(), spawned("reviewer-claude-agentry")),
        (
            "a2".to_string(),
            message("coder-claude-agentry", json!({ "m": "mech-rework" }), 1),
        ),
        (
            "a3".to_string(),
            message("coder-claude-agentry", json!({ "m": "claude-rework" }), 2),
        ),
        (
            "a1".to_string(),
            message("shipper-agentry", json!({ "m": "handoff" }), 3),
        ),
    ];
    // What the in-process all_messages Vec would hold, in arrival order.
    let in_process: Vec<(&str, &str, serde_json::Value, i64)> = vec![
        (
            "reviewer-mechanical-agentry",
            "coder-claude-agentry",
            json!({ "m": "mech-rework" }),
            1,
        ),
        (
            "reviewer-claude-agentry",
            "coder-claude-agentry",
            json!({ "m": "claude-rework" }),
            2,
        ),
        (
            "coder-claude-agentry",
            "shipper-agentry",
            json!({ "m": "handoff" }),
            3,
        ),
    ];
    let out = group_messages_by_role(&entries);

    for role in ["coder-claude-agentry", "shipper-agentry"] {
        let expected: Vec<_> = in_process
            .iter()
            .filter(|(_, to, ..)| *to == role)
            .collect();
        let got = out.get(role).unwrap_or_else(|| panic!("{role} inbox"));
        assert_eq!(got.len(), expected.len(), "{role} inbox length");
        for (g, e) in got.iter().zip(expected.iter()) {
            assert_eq!(g.from, e.0, "{role} from");
            assert_eq!(g.to, e.1, "{role} to");
            assert_eq!(g.payload, e.2, "{role} payload");
            assert_eq!(g.at, ts(e.3), "{role} at");
        }
    }
    assert_eq!(out.len(), 2, "exactly two destination roles");
}

/// #539 phase 5c — daemon-attributed message. The daemon synthesizes
/// the rework findings message and appends it to the trace with the
/// trace `agent` field set to the SOURCE ROLE'S NAME (not a UUID),
/// because no agent process emitted it. That `agent_id` is never in
/// the spawned memo, so the fallback uses it directly as `from`. This
/// reproduces exactly the pre-5c in-process write
/// `RoutedMessage { from: from_ref.name.0, .. }`.
#[test]
fn daemon_attributed_message_uses_agent_id_as_from() {
    // Agent UUIDs (`agt_<hex>`) never collide with role names
    // (`reviewer-claude-agentry`), so a memo miss unambiguously means
    // "daemon synthesized this; the agent field IS the from-role".
    let entries = vec![(
        "reviewer-claude-agentry".to_string(),
        message(
            "coder-claude-agentry",
            json!({ "findings": [{ "severity": "blocker" }] }),
            0,
        ),
    )];
    let out = group_messages_by_role(&entries);
    assert_eq!(
        out["coder-claude-agentry"][0].from,
        "reviewer-claude-agentry"
    );
}

/// Rework round-trip: the reviewer ran (its `spawned` is in the trace
/// under a UUID), then the daemon appends the synthetic findings
/// message attributed to the reviewer's ROLE NAME. The memo holds
/// `uuid → reviewer-claude-agentry` but the synthetic message's
/// `agent_id` is the role name (memo miss → fallback). Both the
/// agent's own messages and the daemon message resolve `from` to the
/// reviewer's role name — identical to the deleted in-process path.
#[test]
fn rework_findings_round_trip_via_daemon_attribution() {
    let entries = vec![
        ("agt_rev01".to_string(), spawned("reviewer-claude-agentry")),
        (
            "reviewer-claude-agentry".to_string(),
            message(
                "coder-claude-agentry",
                json!({ "findings": ["blocker: unapplied verb"] }),
                7,
            ),
        ),
    ];
    let out = group_messages_by_role(&entries);
    let inbox = out
        .get("coder-claude-agentry")
        .expect("rewound coder inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].from, "reviewer-claude-agentry");
    assert_eq!(inbox[0].to, "coder-claude-agentry");
    assert_eq!(
        inbox[0].payload,
        json!({ "findings": ["blocker: unapplied verb"] })
    );
    assert_eq!(inbox[0].at, ts(7));
}

/// Non-Message events (Log, Done, etc.) never enter an inbox.
#[test]
fn non_message_events_are_ignored() {
    let entries = vec![
        ("a1".to_string(), spawned("coder-claude-agentry")),
        (
            "a1".to_string(),
            Event {
                at: ts(1),
                kind: EventKind::Log {
                    level: "info".into(),
                    msg: "working".into(),
                },
            },
        ),
        (
            "a1".to_string(),
            Event {
                at: ts(2),
                kind: EventKind::Done {
                    verdict: EventVerdict::Shipped,
                    reason: None,
                    refusal_count: 0,
                },
            },
        ),
    ];
    assert!(
        group_messages_by_role(&entries).is_empty(),
        "no Message events => empty map"
    );
}

/// The canary's actual shape: every role spawns and ships, no rework
/// => zero routed messages => empty map. This is precisely why the
/// live canary could not validate phase 4 (0 == 0 proves nothing);
/// the empty case is pinned here so the deletion's behavior on the
/// common path is explicit.
#[test]
fn clean_no_rework_brief_routes_zero_messages() {
    let entries = vec![
        ("a1".to_string(), spawned("coder-claude-agentry")),
        ("a2".to_string(), spawned("reviewer-mechanical-agentry")),
        ("a3".to_string(), spawned("shipper-agentry")),
    ];
    assert!(group_messages_by_role(&entries).is_empty());
}

// ---------------------------------------------------------------------------
// #539 phase-6 fence — `daemon::overrides_from_messages` must resolve
// the same `PermitOverrides` the deleted in-process `overrides_for`
// HashMap did: walk inbound messages, match the `from→to` edge whose
// `permit_overrides_from` names a payload key, deserialize it,
// last-write-wins.
// ---------------------------------------------------------------------------

fn role(name: &str) -> RoleRef {
    RoleRef {
        name: RoleName(name.to_string()),
        version: 1,
    }
}

fn routed(from: &str, to: &str, payload: serde_json::Value, at_secs: i64) -> RoutedMessage {
    RoutedMessage {
        from: from.to_string(),
        to: to.to_string(),
        payload,
        at: ts(at_secs),
    }
}

/// Minimal team: `synthesizer → coder` edge carrying
/// `permit_overrides_from = "permit_overrides"`.
fn team_with_override_edge() -> TeamTopology {
    TeamTopology {
        name: TeamName("t".into()),
        version: 1,
        roles: vec![role("synthesizer-agentry"), role("coder-claude-agentry")],
        message_graph: vec![MessageEdge {
            from: role("synthesizer-agentry"),
            to: role("coder-claude-agentry"),
            permit_overrides_from: Some("permit_overrides".to_string()),
            rework_target: None,
            gate_policy: None,
        }],
        terminal_role: role("coder-claude-agentry"),
        max_retries: 2,
        node_classes: Default::default(),
    }
}

#[test]
fn override_extracted_from_matching_edge_key_payload() {
    let team = team_with_override_edge();
    let inbound = vec![routed(
        "synthesizer-agentry",
        "coder-claude-agentry",
        json!({ "permit_overrides": { "fs_write": ["src/a.rs"] } }),
        1,
    )];
    let got = overrides_from_messages(&team, "coder-claude-agentry", &inbound)
        .expect("override resolved");
    assert_eq!(got.fs_write, vec!["src/a.rs".to_string()]);
}

#[test]
fn override_last_write_wins_matches_hashmap_insert() {
    let team = team_with_override_edge();
    let inbound = vec![
        routed(
            "synthesizer-agentry",
            "coder-claude-agentry",
            json!({ "permit_overrides": { "fs_write": ["first.rs"] } }),
            1,
        ),
        routed(
            "synthesizer-agentry",
            "coder-claude-agentry",
            json!({ "permit_overrides": { "fs_write": ["second.rs"] } }),
            2,
        ),
    ];
    let got = overrides_from_messages(&team, "coder-claude-agentry", &inbound)
        .expect("override resolved");
    assert_eq!(
        got.fs_write,
        vec!["second.rs".to_string()],
        "later message wins, matching the deleted HashMap::insert order"
    );
}

#[test]
fn no_override_when_no_edge_or_key_or_wrong_target() {
    let team = team_with_override_edge();
    // Right edge, but payload lacks the key.
    let no_key = vec![routed(
        "synthesizer-agentry",
        "coder-claude-agentry",
        json!({ "something_else": 1 }),
        1,
    )];
    assert!(overrides_from_messages(&team, "coder-claude-agentry", &no_key).is_none());

    // Key present but message addressed to a role with no override edge.
    let wrong_target = vec![routed(
        "synthesizer-agentry",
        "reviewer-claude-agentry",
        json!({ "permit_overrides": { "fs_write": ["x.rs"] } }),
        1,
    )];
    assert!(overrides_from_messages(&team, "reviewer-claude-agentry", &wrong_target).is_none());

    // No inbound at all.
    assert!(overrides_from_messages(&team, "coder-claude-agentry", &[]).is_none());
}

#[test]
fn malformed_override_payload_skipped_not_panicked() {
    let team = team_with_override_edge();
    // `permit_overrides` present but not a PermitOverrides shape.
    let bad = vec![routed(
        "synthesizer-agentry",
        "coder-claude-agentry",
        json!({ "permit_overrides": "not-an-object" }),
        1,
    )];
    assert!(overrides_from_messages(&team, "coder-claude-agentry", &bad).is_none());
}
