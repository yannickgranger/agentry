//! Event — what agents emit on stdout as NDJSON.
//!
//! Every line from an agent's stdout is parsed as one `Event`. The container
//! runner mirrors every event to `agentry:brief:{id}:trace`. The permit broker
//! subscribes and enforces tool allowlists.

use crate::{now, review::ReviewFinding, Ts};
use serde::{Deserialize, Serialize};

/// Verdict emitted by an agent at the end of its run. Distinct from the
/// team-level `crate::verdict::Verdict` — this one travels on the stdout
/// NDJSON wire, the team-level one is persisted as the brief's outcome.
///
/// `ReworkNeeded` and `Rejected` are review-producer verdicts. `ReworkNeeded`
/// signals the daemon to rewind to the upstream worker; the findings
/// themselves travel as separate `EventKind::Finding` events emitted BEFORE
/// the `Done` event and accumulated by the spawner. `Rejected` signals "don't
/// bother retrying".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventVerdict {
    Shipped,
    Failed,
    Escalated,
    ReworkNeeded,
    Rejected,
}

/// Optional structured cause attached to a `Done` event when the verdict was
/// forced by an unexpected exit, timeout, or signal — set by
/// `agentry_role_runtime::DoneGuard`'s Drop impl. Absent on roles that
/// emitted `done` explicitly on their own happy path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneReason {
    /// Short symbolic cause: `"unexpected_exit"` (DoneGuard default),
    /// future: `"timeout"`, `"signal"`, etc.
    pub cause: String,
    /// Exit code captured at the call site if known. Drop-time always emits
    /// `None` because Rust's drop runs before the kernel returns the
    /// process status; roles that detect a specific failure code can call
    /// `emit_done(EventVerdict::Failed, Some(DoneReason { exit_code: Some(_), .. }))`
    /// explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// A tool call attempt — content that the permit broker inspects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// The kind of event. `Done` is terminal; any other kind means the agent is still running.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// Agent is running; payload is freeform.
    Event { payload: serde_json::Value },
    /// Agent is about to call a tool. Permit broker checks against allowlist.
    ToolCall { call: ToolCall },
    /// A tool invocation the agent attempted was refused — either by
    /// `claude --allowedTools` pre-spawn fencing or by the daemon's permit
    /// broker post-hoc audit. `command` carries the concrete invocation
    /// string when available (e.g. the Bash command), `None` when the
    /// refusal was on the tool name alone.
    ToolRefused {
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    /// Agent has a message for another role — content ends up in `to`'s inbox.
    Message {
        to: String,
        payload: serde_json::Value,
    },
    /// Human-readable log line.
    Log { level: String, msg: String },
    /// One review finding; the spawner accumulates findings and attaches
    /// them to the team-level `Verdict` when the `Done` event's verdict is
    /// `ReworkNeeded`.
    Finding { finding: ReviewFinding },
    /// Watchdog-emitted diagnosis: an agent's progress judgment plus the
    /// trace evidence that backed it. XADDed by the watchdog tick to the
    /// agent's brief trace stream so projector watermarks advance and
    /// downstream consumers (dashboards, captain, future commandant
    /// officer council) read it on the same wire as every other event.
    Status {
        agent_id: String,
        ok: bool,
        stuck: bool,
        reason: String,
        selector_name: String,
        evidence_event_ids: Vec<String>,
    },
    /// Agent is done; terminal.
    Done {
        verdict: EventVerdict,
        /// Optional structured cause — set by `DoneGuard` on unexpected
        /// exits, or by callers who know their failure code at the
        /// emit-site. Backwards-compatible: missing in the JSON
        /// deserialises to `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<DoneReason>,
        /// Total `tool_refused` events the agent emitted during its run, set
        /// by `emit_done` reading the static atomic counter incremented at each
        /// `emit_tool_refused` call. Backwards-compatible: missing in JSON
        /// deserialises to 0.
        #[serde(default)]
        refusal_count: u32,
    },
}

/// A stamped event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub at: Ts,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    #[must_use]
    pub fn new(kind: EventKind) -> Self {
        Self { at: now(), kind }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.kind, EventKind::Done { .. })
    }

    #[must_use]
    pub fn verdict(&self) -> Option<EventVerdict> {
        match &self.kind {
            EventKind::Done { verdict, .. } => Some(*verdict),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
