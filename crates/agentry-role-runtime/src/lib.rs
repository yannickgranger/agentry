//! agentry-role-runtime — typed primitives for role binaries.
//!
//! Replaces the `BASH_PRELUDE` heredoc layer with a Rust library every role
//! binary uses to:
//!
//! - Read its startup JSON bundle from stdin (`read_bundle`)
//! - Emit structured NDJSON events on stdout (`emit_event`, `emit_finding`,
//!   `emit_message`, `emit_done`)
//! - Guarantee a terminal `done` event is emitted on every exit path, even
//!   panics or `?`-bubbled errors (`DoneGuard` Drop impl)
//!
//! This is EPIC #161 B0. The `BASH_PRELUDE` EXIT trap from PR #166 (closed
//! superseded) tried to do the same job in bash — repeatedly bitten by
//! pipefail/jq edge cases. The Rust version uses Drop semantics and is
//! structurally immune.
//!
//! Wire format on stdout matches the existing BASH_PRELUDE emit_* shape so the
//! daemon's projector parses the events without changes:
//!
//! ```json
//! {"at":"2026-04-29T01:23:45+00:00","type":"event","payload":{...}}
//! {"at":"...","type":"done","verdict":"shipped"}
//! {"at":"...","type":"done","verdict":"failed","reason":{"cause":"unexpected_exit","exit_code":null}}
//! ```

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use orchestrator_types::{DoneReason, EventVerdict, ReviewFinding};

/// Set by `emit_done`. Read by `DoneGuard::drop`. Static so it works across
/// any task structure inside the role binary.
static DONE_EMITTED: AtomicBool = AtomicBool::new(false);

/// Read the startup JSON bundle from stdin and deserialize into `T`.
pub fn read_bundle<T: DeserializeOwned>() -> anyhow::Result<T> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| anyhow::anyhow!("read stdin: {e}"))?;
    serde_json::from_str(&buf).map_err(|e| anyhow::anyhow!("parse bundle: {e}"))
}

/// Emit one freeform event with a typed payload.
pub fn emit_event(payload: Value) {
    emit_line(json!({
        "at": Utc::now().to_rfc3339(),
        "type": "event",
        "payload": payload,
    }));
}

/// Emit one finding wrapped in the standard event envelope.
pub fn emit_finding(finding: &ReviewFinding) {
    let body = serde_json::to_value(finding).unwrap_or(Value::Null);
    emit_line(json!({
        "at": Utc::now().to_rfc3339(),
        "type": "finding",
        "finding": body,
    }));
}

/// Emit one message routed to a downstream role.
pub fn emit_message(to: &str, payload: Value) {
    emit_line(json!({
        "at": Utc::now().to_rfc3339(),
        "type": "message",
        "to": to,
        "payload": payload,
    }));
}

/// Emit the terminal `done` event with verdict and optional structured reason.
/// Sets the static flag so a `DoneGuard` drop becomes a no-op.
pub fn emit_done(verdict: EventVerdict, reason: Option<DoneReason>) {
    DONE_EMITTED.store(true, Ordering::SeqCst);
    let mut obj = json!({
        "at": Utc::now().to_rfc3339(),
        "type": "done",
        "verdict": verdict_to_str(verdict),
    });
    if let Some(r) = reason {
        if let Ok(v) = serde_json::to_value(&r) {
            obj["reason"] = v;
        }
    }
    emit_line(obj);
}

/// Drop-guard: synthesises `done failed` if no terminal event was emitted by
/// the time the role binary unwinds. Closes the silent-exit failure class
/// that the bash EXIT-trap from PR #166 was trying to catch.
///
/// Construct one at the top of `main`. On normal exit, `emit_done` flips the
/// flag and `drop` no-ops. On panic / unwound `?` / abrupt return, the flag
/// stays unset and `drop` emits a `done failed` carrying
/// `reason: { cause: "unexpected_exit", exit_code: None }`.
///
/// `exit_code` is `None` here because Rust's drop runs before the kernel
/// returns the process status, so the eventual exit code isn't yet
/// observable. Roles that do know their failure code at the call site can
/// invoke `emit_done(EventVerdict::Failed, Some(DoneReason { ... }))`
/// explicitly before letting the guard drop — the explicit emit wins.
pub struct DoneGuard;

impl DoneGuard {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DoneGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        if !DONE_EMITTED.load(Ordering::SeqCst) {
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause: "unexpected_exit".into(),
                    exit_code: None,
                }),
            );
        }
    }
}

// ---------- internal ----------

fn emit_line(value: Value) {
    let line = serde_json::to_string(&value).unwrap_or_default();
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{line}");
    let _ = handle.flush();
}

fn verdict_to_str(v: EventVerdict) -> &'static str {
    // Mirror the existing snake_case wire format from EventVerdict's
    // serde derive. Keeping this as a small const map (rather than going
    // through `serde_json::to_string` and stripping quotes) keeps the hot
    // path allocation-free.
    match v {
        EventVerdict::Shipped => "shipped",
        EventVerdict::Failed => "failed",
        EventVerdict::Escalated => "escalated",
        EventVerdict::ReworkNeeded => "rework_needed",
        EventVerdict::Rejected => "rejected",
    }
}

// Helper kept generic over T for any future structured payload roles.
#[allow(dead_code)]
fn emit_typed_payload<T: Serialize>(value: &T) -> Option<Value> {
    serde_json::to_value(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_to_str_matches_serde_snake_case() {
        // Verify the const map agrees with serde's snake_case rename.
        for v in [
            EventVerdict::Shipped,
            EventVerdict::Failed,
            EventVerdict::Escalated,
            EventVerdict::ReworkNeeded,
            EventVerdict::Rejected,
        ] {
            let serde_form = serde_json::to_string(&v).expect("ser");
            // serde_json::to_string of an enum variant emits a JSON string
            // including quotes — strip them for comparison.
            let unquoted = serde_form.trim_matches('"');
            assert_eq!(verdict_to_str(v), unquoted, "drift for {v:?}");
        }
    }

    #[test]
    fn emit_done_sets_flag() {
        DONE_EMITTED.store(false, Ordering::SeqCst);
        emit_done(EventVerdict::Shipped, None);
        assert!(DONE_EMITTED.load(Ordering::SeqCst));
    }

    #[test]
    fn done_guard_default_is_unemitted() {
        DONE_EMITTED.store(false, Ordering::SeqCst);
        let _g = DoneGuard::new();
        assert!(!DONE_EMITTED.load(Ordering::SeqCst));
        // Dropping `_g` here writes a `done failed` line to stdout. We don't
        // capture stdout in this unit test (would require thread-local
        // redirection); the integration test in tests/done_guard.rs does
        // that subprocess-level check.
    }
}
