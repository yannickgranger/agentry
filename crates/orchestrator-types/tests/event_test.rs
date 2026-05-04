use orchestrator_types::{DoneReason, Event, EventKind, EventVerdict, ToolCall};
use serde_json::json;

#[test]
fn event_roundtrip_simple() {
    let e = Event::new(EventKind::Event {
        payload: json!({"msg": "hello"}),
    });
    let s = serde_json::to_string(&e).expect("ser");
    let back: Event = serde_json::from_str(&s).expect("de");
    assert_eq!(e, back);
    assert!(!e.is_terminal());
}

#[test]
fn done_event_is_terminal() {
    let e = Event::new(EventKind::Done {
        verdict: EventVerdict::Shipped,
        reason: None,
        refusal_count: 0,
    });
    assert!(e.is_terminal());
    assert_eq!(e.verdict(), Some(EventVerdict::Shipped));
}

#[test]
fn done_event_with_reason_roundtrips() {
    let e = Event::new(EventKind::Done {
        verdict: EventVerdict::Failed,
        reason: Some(DoneReason {
            cause: "unexpected_exit".into(),
            exit_code: Some(5),
        }),
        refusal_count: 0,
    });
    let s = serde_json::to_string(&e).expect("ser");
    let back: Event = serde_json::from_str(&s).expect("de");
    assert_eq!(e, back);
    assert!(s.contains("\"reason\""));
    assert!(s.contains("\"cause\":\"unexpected_exit\""));
}

#[test]
fn done_event_legacy_json_deserializes_with_reason_none() {
    // Old wire format — no reason field — must still parse.
    let line = r#"{"at":"2026-04-23T10:00:01Z","type":"done","verdict":"shipped"}"#;
    let e: Event = serde_json::from_str(line).expect("parse");
    match e.kind {
        EventKind::Done {
            verdict,
            reason,
            refusal_count,
        } => {
            assert_eq!(verdict, EventVerdict::Shipped);
            assert!(
                reason.is_none(),
                "legacy JSON must deserialize reason as None"
            );
            assert_eq!(
                refusal_count, 0,
                "missing refusal_count field must default to 0"
            );
        }
        _ => panic!("wrong kind"),
    }
}

#[test]
fn done_event_with_refusal_count_roundtrips() {
    let e = Event::new(EventKind::Done {
        verdict: EventVerdict::Shipped,
        reason: None,
        refusal_count: 5,
    });
    let s = serde_json::to_string(&e).expect("ser");
    let back: Event = serde_json::from_str(&s).expect("de");
    assert_eq!(e, back);
    assert!(s.contains("\"refusal_count\":5"));
}

#[test]
fn tool_call_event_serializes() {
    let e = Event::new(EventKind::ToolCall {
        call: ToolCall {
            tool: "read".into(),
            args: json!({"path": "/workspace/README.md"}),
        },
    });
    let s = serde_json::to_string(&e).expect("ser");
    assert!(s.contains("\"type\":\"tool_call\""));
}

#[test]
fn ndjson_line_parses() {
    // Simulating what the container runner reads from stdout.
    let line = r#"{"at":"2026-04-23T10:00:00Z","type":"event","payload":{"msg":"hello"}}"#;
    let e: Event = serde_json::from_str(line).expect("parse");
    match e.kind {
        EventKind::Event { payload } => {
            assert_eq!(payload["msg"], "hello");
        }
        _ => panic!("wrong kind"),
    }
}

#[test]
fn done_line_parses() {
    let line = r#"{"at":"2026-04-23T10:00:01Z","type":"done","verdict":"shipped"}"#;
    let e: Event = serde_json::from_str(line).expect("parse");
    assert_eq!(e.verdict(), Some(EventVerdict::Shipped));
}

#[test]
fn tool_refused_event_roundtrips() {
    let e = Event::new(EventKind::ToolRefused {
        tool: "Bash".into(),
        command: Some("cargo check".into()),
    });
    let s = serde_json::to_string(&e).expect("ser");
    let back: Event = serde_json::from_str(&s).expect("de");
    assert_eq!(e, back);
    assert!(s.contains("\"type\":\"tool_refused\""));
    assert!(s.contains("\"tool\":\"Bash\""));
    assert!(s.contains("\"command\":\"cargo check\""));
}

#[test]
fn event_kind_match_is_exhaustive() {
    // Compile-time check: every variant of `EventKind`, including
    // `ToolRefused`, is named below. Adding a variant without updating
    // this test is a hard compile error.
    fn classify(k: &EventKind) -> &'static str {
        match k {
            EventKind::Event { .. } => "event",
            EventKind::ToolCall { .. } => "tool_call",
            EventKind::ToolRefused { .. } => "tool_refused",
            EventKind::Message { .. } => "message",
            EventKind::Log { .. } => "log",
            EventKind::Finding { .. } => "finding",
            EventKind::Status { .. } => "status",
            EventKind::RetryRequested { .. } => "retry_requested",
            EventKind::Done { .. } => "done",
        }
    }
    assert_eq!(
        classify(&EventKind::ToolRefused {
            tool: "Read".into(),
            command: None,
        }),
        "tool_refused"
    );
}

/// Operator-initiated retry signal: a trace-stream entry of the
/// shape `{"type":"retry_requested","actor":"...","reason":"..."}`
/// (top-level) decodes into `EventKind::RetryRequested` with the
/// fields populated. The lifecycle EventSource adapter consumes
/// this shape; the producer (operator CLI / dashboard) is out of
/// scope here.
#[test]
fn retry_requested_event_decodes_top_level_shape() {
    let line = r#"{"at":"2026-04-23T10:00:00Z","type":"retry_requested","actor":"alice","reason":"flake on CI"}"#;
    let e: Event = serde_json::from_str(line).expect("parse retry_requested");
    match &e.kind {
        EventKind::RetryRequested { actor, reason } => {
            assert_eq!(actor, "alice");
            assert_eq!(reason, "flake on CI");
        }
        other => panic!("expected EventKind::RetryRequested, got {other:?}"),
    }
    assert!(!e.is_terminal());
    assert_eq!(e.verdict(), None);
}

#[test]
fn retry_requested_event_roundtrips() {
    let e = Event::new(EventKind::RetryRequested {
        actor: "ops".into(),
        reason: "rerun".into(),
    });
    let s = serde_json::to_string(&e).expect("ser");
    assert!(s.contains("\"type\":\"retry_requested\""), "got: {s}");
    let back: Event = serde_json::from_str(&s).expect("de");
    assert_eq!(e, back);
}

#[test]
fn status_event_serializes_with_type_tag() {
    let e = Event::new(EventKind::Status {
        agent_id: "agt_x".into(),
        ok: true,
        stuck: false,
        reason: "progressing".into(),
        selector_name: "all_running".into(),
        evidence_event_ids: vec!["1234-0".into(), "5678-0".into()],
    });
    let s = serde_json::to_string(&e).expect("ser");
    assert!(s.contains("\"type\":\"status\""), "got: {s}");
    assert!(!e.is_terminal());
    assert_eq!(e.verdict(), None);
    let back: Event = serde_json::from_str(&s).expect("de");
    assert_eq!(e, back);
}
