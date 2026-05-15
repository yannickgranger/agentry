//! #539 phase-5 fence — `daemon::group_messages_by_role` must
//! reconstruct the exact `RoutedMessage` set the in-process
//! `all_messages` accumulator + `.filter(|m| m.to == role).cloned()`
//! would produce. Pinning this equivalence is the precondition for
//! deleting `all_messages` in phase 5b/5c: without a deterministic
//! proof, the swap to trace-derivation is unverifiable (a clean
//! no-rework brief routes zero messages, so a live canary proves
//! nothing about the message path — see the 5a PR body).

use chrono::{TimeZone, Utc};
use orchestrator_runtime::daemon::group_messages_by_role;
use orchestrator_types::{Event, EventKind, EventVerdict};
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

/// Guarded precondition: `spawner::run_agent` appends the `spawned`
/// trace entry at container start, before forwarding any agent
/// stdout — so every `Message` is preceded by its sender's `spawned`.
/// If that invariant regressed (spawn entry dropped), `from` degrades
/// to `""`. Pinned so the regression is caught, not silently shipped.
#[test]
fn message_without_prior_spawned_yields_empty_from_guarded_precondition() {
    let entries = vec![(
        "orphan_agent".to_string(),
        message("coder-claude-agentry", json!({ "x": 1 }), 0),
    )];
    let out = group_messages_by_role(&entries);
    assert_eq!(out["coder-claude-agentry"][0].from, "");
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
