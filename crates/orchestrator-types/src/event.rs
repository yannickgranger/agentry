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
    /// Operator-initiated retry signal: surfaces on the brief's trace
    /// stream as `{"type":"retry_requested","actor":"...","reason":"..."}`
    /// and is decoded by the lifecycle EventSource into
    /// `BriefEvent::RetryRequested`. The producer (operator CLI,
    /// dashboard button, external script) is out of scope here — this
    /// variant exists so the EventSource can decode the entry when
    /// something else writes it.
    RetryRequested { actor: String, reason: String },
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
