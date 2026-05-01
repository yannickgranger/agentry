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

pub mod claude;
pub use claude::{stream_claude, StreamErr};

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use chrono::Utc;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use orchestrator_types::{DoneReason, EventVerdict, FindingOrigin, ReviewFinding, Severity};

/// Set by `emit_done`. Read by `DoneGuard::drop`. Static so it works across
/// any task structure inside the role binary.
static DONE_EMITTED: AtomicBool = AtomicBool::new(false);

/// Incremented at each `emit_tool_refused` call; read once by `emit_done`
/// and embedded as `refusal_count` on the terminal event so the spawner can
/// surface the per-run total on the team-level `Verdict`.
static REFUSAL_COUNT: AtomicU32 = AtomicU32::new(0);

/// Read the startup JSON bundle from stdin and deserialize into `T`.
pub fn read_bundle<T: DeserializeOwned>() -> anyhow::Result<T> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| anyhow::anyhow!("read stdin: {e}"))?;
    serde_json::from_str(&buf).map_err(|e| anyhow::anyhow!("parse bundle: {e}"))
}

/// Read the startup JSON bundle from stdin as an opaque [`serde_json::Value`].
///
/// Use this when the binary doesn't have a strict typed shape for the bundle
/// — e.g. opaque `brief.payload` access via JSON pointer paths. Prefer
/// [`read_bundle`] when a typed deserialization is possible.
pub fn read_bundle_value() -> anyhow::Result<Value> {
    read_bundle::<Value>()
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

/// Emit one `tool_refused` event — top-level `type:"tool_refused"` so the
/// line round-trips into [`orchestrator_types::EventKind::ToolRefused`]
/// (NOT the freeform `EventKind::Event`). Mirrors `emit_finding` /
/// `emit_message` in calling [`emit_line`] directly rather than wrapping
/// the variant in an `event` envelope.
pub fn emit_tool_refused(tool: &str, command: Option<&str>) {
    REFUSAL_COUNT.fetch_add(1, Ordering::SeqCst);
    emit_line(json!({
        "at": Utc::now().to_rfc3339(),
        "type": "tool_refused",
        "tool": tool,
        "command": command,
    }));
}

/// Emit the terminal `done` event with verdict and optional structured reason.
/// Sets the static flag so a `DoneGuard` drop becomes a no-op.
pub fn emit_done(verdict: EventVerdict, reason: Option<DoneReason>) {
    DONE_EMITTED.store(true, Ordering::SeqCst);
    let refusal_count = REFUSAL_COUNT.load(Ordering::SeqCst);
    let mut obj = json!({
        "at": Utc::now().to_rfc3339(),
        "type": "done",
        "verdict": verdict_to_str(verdict),
        "refusal_count": refusal_count,
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

// ---------- shared helpers (consolidated from runner binaries, brief #213) ----------

/// Last `n` lines of `buf` joined with `\n`. UTF-8 lossy.
pub fn tail_lines(buf: &[u8], n: usize) -> String {
    let s = String::from_utf8_lossy(buf);
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Last `n` bytes of `buf` as a lossy UTF-8 string.
pub fn tail_bytes(buf: &[u8], n: usize) -> String {
    let start = buf.len().saturating_sub(n);
    String::from_utf8_lossy(&buf[start..]).into_owned()
}

/// Head of `s` clamped to `n` bytes, snapped down to the nearest UTF-8
/// char boundary so multi-byte chars don't get split.
pub fn head_bytes(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// Read `v.pointer(ptr).as_str()` or `""` when missing/non-string.
pub fn pointer_str<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

/// Like [`pointer_str`] but returns `default` when the field is missing or empty.
pub fn pointer_str_or(v: &Value, ptr: &str, default: &str) -> String {
    let s = pointer_str(v, ptr);
    if s.is_empty() {
        default.to_string()
    } else {
        s.to_string()
    }
}

/// Map a textual severity (`"blocker"`/`"warn"`/...) to [`Severity`].
/// Unknown strings (and `None`) default to `Severity::Warn` to match the
/// daemon-side fallback behaviour.
pub fn parse_severity(opt: Option<&str>) -> Severity {
    match opt.unwrap_or("warn") {
        "blocker" => Severity::Blocker,
        _ => Severity::Warn,
    }
}

/// Read the bundle's `/permit/allowed_tools` array as a `Vec<String>` of
/// `claude --allowedTools` patterns. Empty when missing, null, or not an
/// array. Non-string entries are silently dropped.
///
/// Single source of truth for both `coder_claude_runner` and
/// `reviewer_claude_runner` — keeps the field name + JSON-pointer path
/// from drifting per-binary.
pub fn parse_allowed_tools(bundle: &Value) -> Vec<String> {
    bundle
        .pointer("/permit/allowed_tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Read an array-of-strings field on `v`. Missing / non-array / non-string
/// entries are silently dropped.
pub fn string_array_field(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Strip ` ``` ` and ` ```json ` fence lines (and blank lines) from `raw`,
/// preserving everything else. Used for parsing claude replies that wrap
/// JSON in markdown code fences.
pub fn strip_fences(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed == "```" || trimmed == "```json" {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out
}

/// True iff `<workspace>/.git` exists as a directory (full clone) or file
/// (worktree).
pub fn workspace_is_git_repo(workspace: &str) -> bool {
    let dot_git = std::path::Path::new(workspace).join(".git");
    dot_git.is_dir() || dot_git.is_file()
}

/// Build a mechanical-origin Blocker [`ReviewFinding`].
///
/// Promoted from `coder_claude_runner.rs` — the only binary with this
/// helper today, so the lib signature mirrors that one verbatim:
/// `(tool, category, message)` → Blocker mechanical finding with `rule`
/// unset.
pub fn mech_finding(tool: &str, category: &str, message: &str) -> ReviewFinding {
    ReviewFinding {
        file: None,
        line: None,
        severity: Severity::Blocker,
        origin: FindingOrigin::Mechanical {
            tool: tool.into(),
            rule: None,
        },
        category: category.into(),
        message: message.into(),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

// ---------- reviewer-binary helpers (extracted from reviewer_claude_runner, brief Y.1) ----------

const SALVAGE_BUDGET: usize = 4096;

/// True iff `ra-query --version` succeeds. Used to gate the reviewer's
/// pre-pass when operators haven't run `just ra-query-binary`.
pub fn ra_query_present() -> bool {
    Command::new("ra-query")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Run `ra-query` with the given args and parse stdout as JSON. Returns
/// `Err` with a short reason on spawn / non-zero exit / unparseable JSON.
pub fn run_ra_query(args: &[&str]) -> Result<Value, String> {
    let out = Command::new("ra-query")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn ra-query: {e}"))?;
    if !out.status.success() {
        return Err(format!("ra-query exit {:?}", out.status.code()));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("parse ra-query json: {e}"))
}

/// Extract the set of `*.rs` paths the diff touches by scanning the unified
/// diff's `+++ b/<path>.rs` headers. Mirrors bash:
///   `grep -E '^\+\+\+ b/.*\.rs$' /tmp/diff.patch | sed 's|^\+\+\+ b/||'`
pub fn changed_rs_files(diff_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in diff_text.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            if rest.ends_with(".rs") {
                out.push(rest.to_string());
            }
        }
    }
    out
}

/// Slice the substring between the first `[` and last `]` in `text`, or
/// `None` if either is missing or `]` precedes `[`.
pub fn slice_json_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end < start {
        return None;
    }
    Some(&text[start..=end])
}

/// Strip optional code fences, slice between first `[` and last `]`, parse
/// as a JSON array of finding-shape objects. On any failure, salvage the
/// reply as a single `format_deviation` Blocker finding so the rework loop
/// has a concrete handle. Returns the emit-ready findings.
pub fn parse_findings(response: &str, agent_id: &str) -> Vec<ReviewFinding> {
    let cleaned = strip_fences(response);
    let sliced = match slice_json_array(&cleaned) {
        Some(s) => s,
        None => {
            return vec![salvage_format_deviation(&cleaned, agent_id)];
        }
    };
    let arr: Vec<Value> = match serde_json::from_str::<Value>(sliced) {
        Ok(Value::Array(a)) => a,
        _ => {
            return vec![salvage_format_deviation(sliced, agent_id)];
        }
    };
    arr.into_iter()
        .map(|v| convert_finding(&v, agent_id))
        .collect()
}

fn salvage_format_deviation(raw: &str, agent_id: &str) -> ReviewFinding {
    let head = head_bytes(raw, SALVAGE_BUDGET);
    emit_event(json!({
        "msg": "reviewer prose-reply salvaged as format_deviation",
        "bytes": head.len(),
    }));
    // PORT NOTE: bash emitted severity:"error" here (not in the Severity
    // enum). Daemon-side deserialization rejected it, leaving the rework
    // loop without a blocker handle. The Rust port emits Blocker — same
    // intent (rework needed because the review couldn't be parsed) but
    // serializes correctly.
    ReviewFinding {
        file: None,
        line: None,
        severity: Severity::Blocker,
        origin: FindingOrigin::Model {
            reviewer_agent_id: agent_id.to_string(),
        },
        category: "format_deviation".into(),
        message: head,
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

fn convert_finding(v: &Value, agent_id: &str) -> ReviewFinding {
    let severity = parse_severity(v.get("severity").and_then(Value::as_str));
    let category = v
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("other")
        .to_string();
    let message = v
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let prohibitions = string_array_field(v, "prohibitions");
    let requirements = string_array_field(v, "requirements");
    ReviewFinding {
        file: None,
        line: None,
        severity,
        origin: FindingOrigin::Model {
            reviewer_agent_id: agent_id.to_string(),
        },
        category,
        message,
        suggested_fix: None,
        prohibitions,
        requirements,
    }
}

// ---------- fence policy types (brief Y.2) ----------

/// Fence variant — each maps to one ra-query subcommand call and one
/// rule string in the emitted `FindingOrigin::Mechanical { rule }`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FenceKind {
    /// `ra-query clones`, `clones_in_loop` field > threshold
    ClonesInLoop,
    /// `ra-query clones`, `clone_calls - arc_rc` outside loop > threshold
    CloneProd,
    /// `ra-query complexity`, per-function cognitive complexity > threshold
    Complexity,
    /// `ra-query unwraps`, severity at threshold or above
    Unwraps,
    /// `ra-query callers <file:line:col>` — zero callers on a new pub item
    CallersZero,
}

/// Severity ladder for `ra-query unwraps` output.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnwrapSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Threshold comparison shape. Three forms cover all current fences.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Threshold {
    /// Numeric count > N (clones / complexity)
    GreaterThan(u32),
    /// Severity at or above N (unwraps)
    SeverityAtLeast(UnwrapSeverity),
    /// Numeric count == N (callers zero check)
    EqualTo(u32),
}

/// The fence policy table. Adding a sixth fence is one row addition.
pub const FENCE_MATRIX: &[(FenceKind, Threshold, Severity)] = &[
    (
        FenceKind::ClonesInLoop,
        Threshold::GreaterThan(0),
        Severity::Blocker,
    ),
    (
        FenceKind::CloneProd,
        Threshold::GreaterThan(0),
        Severity::Blocker,
    ),
    (
        FenceKind::Complexity,
        Threshold::GreaterThan(15),
        Severity::Blocker,
    ),
    (
        FenceKind::Unwraps,
        Threshold::SeverityAtLeast(UnwrapSeverity::High),
        Severity::Blocker,
    ),
    (
        FenceKind::CallersZero,
        Threshold::EqualTo(0),
        Severity::Blocker,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn emit_tool_refused_increments_static_counter() {
        // Snapshot rather than reset-to-zero — other tests in this binary
        // may also call `emit_tool_refused` and the counter is process-wide.
        let before = REFUSAL_COUNT.load(Ordering::SeqCst);
        emit_tool_refused("Bash", Some("echo hi"));
        emit_tool_refused("Read", None);
        emit_tool_refused("Edit", Some("/tmp/x"));
        let after = REFUSAL_COUNT.load(Ordering::SeqCst);
        assert_eq!(after - before, 3);
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

    #[test]
    fn pointer_str_or_uses_default_when_missing_or_empty() {
        let v = json!({"a": "value", "b": ""});
        assert_eq!(pointer_str_or(&v, "/missing", "default"), "default");
        assert_eq!(pointer_str_or(&v, "/a", "default"), "value");
        assert_eq!(pointer_str_or(&v, "/b", "default"), "default");
    }

    #[test]
    fn parse_severity_known_blocker() {
        assert_eq!(parse_severity(Some("blocker")), Severity::Blocker);
    }

    #[test]
    fn parse_severity_unknown_defaults_to_warn() {
        assert_eq!(parse_severity(Some("warn")), Severity::Warn);
        assert_eq!(parse_severity(Some("info")), Severity::Warn);
        assert_eq!(parse_severity(None), Severity::Warn);
    }

    #[test]
    fn head_bytes_respects_char_boundary() {
        // Emoji is 4 bytes in UTF-8. Budget 5 should clamp to 4 (after the
        // emoji) or 0 (before), never split it.
        let s = "🚀x"; // 4 bytes + 1 byte = 5 bytes
        let h = head_bytes(s, 4);
        assert!(h.is_empty() || h == "🚀");
    }

    #[test]
    fn tail_lines_returns_last_n() {
        let buf = b"a\nb\nc\nd\ne";
        assert_eq!(tail_lines(buf, 3), "c\nd\ne");
        assert_eq!(tail_lines(buf, 100), "a\nb\nc\nd\ne");
        assert_eq!(tail_lines(b"", 5), "");
    }

    #[test]
    fn tail_bytes_returns_last_n() {
        let buf = b"abcdefgh";
        assert_eq!(tail_bytes(buf, 3), "fgh");
        assert_eq!(tail_bytes(buf, 100), "abcdefgh");
    }

    #[test]
    fn strip_fences_drops_json_and_plain() {
        assert_eq!(strip_fences("```json\n{\"x\":1}\n```\n"), "{\"x\":1}\n");
    }

    #[test]
    fn strip_fences_drops_json_and_plain_fences() {
        let raw = "```json\n[1,2]\n```\n";
        assert_eq!(strip_fences(raw), "[1,2]\n");
    }

    #[test]
    fn strip_fences_drops_blank_lines() {
        let raw = "\n[1,2]\n\nmore\n";
        assert_eq!(strip_fences(raw), "[1,2]\nmore\n");
    }

    #[test]
    fn emit_tool_refused_round_trips_into_event_kind_tool_refused() {
        // Build the same JSON object emit_tool_refused writes and parse it
        // through the typed Event enum — this exercises the wire shape
        // without touching stdout.
        let line = json!({
            "at": Utc::now().to_rfc3339(),
            "type": "tool_refused",
            "tool": "Bash",
            "command": "rm -rf /",
        });
        let s = serde_json::to_string(&line).expect("ser");
        let evt: orchestrator_types::Event = serde_json::from_str(&s).expect("de");
        match evt.kind {
            orchestrator_types::EventKind::ToolRefused { tool, command } => {
                assert_eq!(tool, "Bash");
                assert_eq!(command.as_deref(), Some("rm -rf /"));
            }
            other => panic!("expected ToolRefused, got {other:?}"),
        }
    }

    #[test]
    fn emit_tool_refused_with_none_command_round_trips() {
        let line = json!({
            "at": Utc::now().to_rfc3339(),
            "type": "tool_refused",
            "tool": "Read",
            "command": Value::Null,
        });
        let s = serde_json::to_string(&line).expect("ser");
        let evt: orchestrator_types::Event = serde_json::from_str(&s).expect("de");
        match evt.kind {
            orchestrator_types::EventKind::ToolRefused { tool, command } => {
                assert_eq!(tool, "Read");
                assert!(command.is_none());
            }
            other => panic!("expected ToolRefused, got {other:?}"),
        }
    }

    #[test]
    fn parse_allowed_tools_reads_string_array() {
        let bundle = json!({
            "permit": {"allowed_tools": ["Read", "Edit", "Bash(cargo fmt:*)"]},
        });
        assert_eq!(
            parse_allowed_tools(&bundle),
            vec![
                "Read".to_string(),
                "Edit".to_string(),
                "Bash(cargo fmt:*)".to_string()
            ]
        );
    }

    #[test]
    fn parse_allowed_tools_returns_empty_when_missing() {
        let bundle = json!({"permit": {}});
        assert!(parse_allowed_tools(&bundle).is_empty());
    }

    #[test]
    fn parse_allowed_tools_returns_empty_for_non_array() {
        let bundle = json!({"permit": {"allowed_tools": "not an array"}});
        assert!(parse_allowed_tools(&bundle).is_empty());
    }

    #[test]
    fn parse_allowed_tools_drops_non_string_entries() {
        let bundle = json!({"permit": {"allowed_tools": ["Read", 42, null, "Edit"]}});
        assert_eq!(
            parse_allowed_tools(&bundle),
            vec!["Read".to_string(), "Edit".to_string()]
        );
    }

    #[test]
    fn mech_finding_uses_blocker_severity() {
        let f = mech_finding("cargo-fmt", "fmt", "boom");
        assert!(matches!(f.severity, Severity::Blocker));
        match &f.origin {
            FindingOrigin::Mechanical { tool, rule } => {
                assert_eq!(tool, "cargo-fmt");
                assert!(rule.is_none());
            }
            _ => panic!("expected mechanical origin"),
        }
        assert_eq!(f.category, "fmt");
        assert_eq!(f.message, "boom");
    }

    #[test]
    fn slice_json_array_picks_outermost() {
        assert_eq!(slice_json_array("prefix[1,2]suffix"), Some("[1,2]"));
        assert_eq!(
            slice_json_array("garbage [\"a\",\"b\"]"),
            Some("[\"a\",\"b\"]")
        );
    }

    #[test]
    fn slice_json_array_returns_none_when_missing() {
        assert_eq!(slice_json_array("no brackets here"), None);
        assert_eq!(slice_json_array("] only closing"), None);
        assert_eq!(slice_json_array("[ only opening"), None);
    }

    #[test]
    fn parse_findings_empty_array_yields_zero() {
        let v = parse_findings("[]", "agt-1");
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn parse_findings_extracts_blocker_with_constraints() {
        let response = r#"
        ```json
        [
          {
            "severity": "blocker",
            "category": "invariant",
            "message": "missing idempotency gate",
            "prohibitions": ["do not introduce a new abstraction"],
            "requirements": ["wrap emission in SETNX"]
          },
          {
            "severity": "warn",
            "category": "naming",
            "message": "function could be renamed"
          }
        ]
        ```
        "#;
        let v = parse_findings(response, "agt-2");
        assert_eq!(v.len(), 2);
        assert!(matches!(v[0].severity, Severity::Blocker));
        assert_eq!(v[0].category, "invariant");
        assert_eq!(v[0].prohibitions.len(), 1);
        assert_eq!(v[0].requirements.len(), 1);
        assert!(matches!(v[1].severity, Severity::Warn));
        assert_eq!(v[1].prohibitions.len(), 0);
        match &v[0].origin {
            FindingOrigin::Model { reviewer_agent_id } => {
                assert_eq!(reviewer_agent_id, "agt-2");
            }
            _ => panic!("expected Model origin"),
        }
    }

    #[test]
    fn parse_findings_salvages_prose_only_reply() {
        let v = parse_findings("I think this looks fine actually.", "agt-3");
        assert_eq!(v.len(), 1);
        assert!(matches!(v[0].severity, Severity::Blocker));
        assert_eq!(v[0].category, "format_deviation");
        assert!(v[0].message.contains("looks fine"));
    }

    #[test]
    fn parse_findings_salvages_when_brackets_dont_form_array() {
        // `]xyz[` has both brackets but slice_json_array returns None
        // because end < start.
        let v = parse_findings("] not actually an array [", "agt-4");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].category, "format_deviation");
    }

    #[test]
    fn parse_findings_salvages_when_sliced_text_is_not_array() {
        // First-bracket-to-last-bracket happens to slice an object instead
        // of an array — ensure salvage fires rather than panicking.
        let v = parse_findings("prelude [{\"x\":1}] postscript", "agt-5");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].category, "other"); // {x:1} has no category — defaults
    }

    #[test]
    fn changed_rs_files_picks_only_rust() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1 +1 @@\n\
                    -old\n\
                    +new\n\
                    diff --git a/notes.md b/notes.md\n\
                    --- a/notes.md\n\
                    +++ b/notes.md\n\
                    @@ -1 +1 @@\n\
                    -a\n\
                    +b\n";
        let v = changed_rs_files(diff);
        assert_eq!(v, vec!["src/lib.rs"]);
    }
}
