//! Event — what agents emit on stdout as NDJSON.
//!
//! Every line from an agent's stdout is parsed as one `Event`. The container
//! runner mirrors every event to `agentry:brief:{id}:trace`. The permit broker
//! subscribes and enforces tool allowlists.

use crate::{now, Ts};
use serde::{Deserialize, Serialize};

/// Verdict emitted by an agent at the end of its run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Shipped,
    Failed,
    Escalated,
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
    /// Agent has a message for another role — content ends up in `to`'s inbox.
    Message {
        to: String,
        payload: serde_json::Value,
    },
    /// Human-readable log line.
    Log { level: String, msg: String },
    /// Agent is done; terminal.
    Done { verdict: Verdict },
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
    pub fn verdict(&self) -> Option<Verdict> {
        match &self.kind {
            EventKind::Done { verdict } => Some(*verdict),
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
            verdict: Verdict::Shipped,
        });
        assert!(e.is_terminal());
        assert_eq!(e.verdict(), Some(Verdict::Shipped));
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
        assert_eq!(e.verdict(), Some(Verdict::Shipped));
    }
}
