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

pub mod archaeologist;
pub mod ci_watcher_runner;
pub mod claude;
pub mod planner;
pub mod pr_rebaser;
pub mod precommit_gate;
pub mod shipper_runner;
pub use claude::{stream_claude, StreamErr};
pub use precommit_gate::{
    decide_gate, derive_fqn, is_orphan_pub_item, orphan_pub_item_finding, parse_allowlist_toml,
    parse_new_pub_items, GateDecision, NewPubItem, PublicApiAllowlist, GATE_CATEGORY, GATE_SOURCE,
};

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
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

/// Build the reviewer-mechanical combined-output snippet. Mirrors the bash
/// `printf '%s\n---stdout---\n%s' "$err" "$out" | head -c <n>` exactly: BYTE
/// (not char) truncation with a fixed `\n---stdout---\n` separator, decoded
/// `String::from_utf8_lossy` so a byte-cut inside a multi-byte UTF-8 sequence
/// produces a replacement char rather than panic.
pub fn build_reviewer_combined(err_tail: &str, out_tail: &str, n: usize) -> String {
    let combined = format!("{err_tail}\n---stdout---\n{out_tail}");
    let head = combined.as_bytes().chunks(n).next().unwrap_or_default();
    String::from_utf8_lossy(head).into_owned()
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

/// Which AC verifier provider an `ac-verifier-runner` invocation runs.
///
/// Each variant maps 1:1 to a bind-mounted host binary on `PATH` inside the
/// container. The string returned by `binary_name` is used both for `Command`
/// dispatch and for human-readable msg prefixes in degradation events
/// (preserving the bash scripts' wording).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Gemini,
    Grok,
}

impl Provider {
    pub fn binary_name(self) -> &'static str {
        match self {
            Provider::Claude => "ac-verifier",
            Provider::Gemini => "ac-verifier-gemini",
            Provider::Grok => "ac-verifier-grok",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Provider::Claude),
            "gemini" => Some(Provider::Gemini),
            "grok" => Some(Provider::Grok),
            _ => None,
        }
    }
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

/// Drop reviewer-emitted Blocker findings whose `message`, `requirements`,
/// AND `prohibitions` are ALL empty — the reviewer's prompt promises a
/// structured Blocker, an empty one is a parse failure, not a real defect
/// (#311). Returns the count of dropped findings so the caller can log
/// and decide whether the surviving verdict still has a real Blocker.
pub fn drop_empty_blocker_findings(findings: &mut Vec<ReviewFinding>) -> usize {
    let before = findings.len();
    findings.retain(|f| {
        !matches!(f.severity, Severity::Blocker)
            || !f.message.is_empty()
            || !f.requirements.is_empty()
            || !f.prohibitions.is_empty()
    });
    before - findings.len()
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

// ---------- run_fence pipeline (brief Y.3) ----------

/// Run the deterministic mechanical fence pipeline against the diff between
/// `origin/<base_branch>` and `HEAD` in `workspace`. For each changed `*.rs`
/// file outside `tests/`, invokes `ra-query clones | complexity | unwraps`
/// and folds the JSON output into [`ReviewFinding`]s with
/// [`FindingOrigin::Mechanical`].
///
/// Fail-closed (Y.5): if `ra-query` is unavailable in the reviewer container
/// or any `run_ra_query` invocation fails (spawn error, non-zero exit, parse
/// error), `run_fence` returns exactly ONE Blocker finding with
/// `rule = Some("ra_query_unavailable")` and discards any partial findings.
/// One Blocker is the substrate-broken signal; further findings would be
/// unreliable. Pre-fence worktree cleanup hiccups after a successful fence
/// pass are best-effort and do NOT override collected findings.
pub fn run_fence(workspace: &Path, base_branch: &str) -> Vec<ReviewFinding> {
    let fail_closed = |reason: &str, detail: &str| -> Vec<ReviewFinding> {
        vec![ReviewFinding {
            file: None,
            line: None,
            severity: Severity::Blocker,
            origin: FindingOrigin::Mechanical {
                tool: "ra-query".into(),
                rule: Some("ra_query_unavailable".into()),
            },
            category: "fence".into(),
            message: format!(
                "ra-query unavailable in reviewer container ({reason}): {detail} — substrate must be repaired before briefs can ship"
            ),
            suggested_fix: None,
            prohibitions: Vec::new(),
            requirements: Vec::new(),
        }]
    };

    let changed = match changed_rs_files_via_git(workspace, base_branch) {
        Ok(v) => v,
        Err(e) => return fail_closed("git_diff_failed", &e),
    };

    let mut findings = Vec::new();
    for f in &changed {
        let abs = workspace.join(f);
        if !abs.is_file() {
            continue;
        }
        let abs_s = abs.to_string_lossy().into_owned();

        let json = match run_ra_query(&["clones", &abs_s, "--active-only", "--format", "json"]) {
            Ok(v) => v,
            Err(e) => return fail_closed("clones_query_failed", &e),
        };
        findings.extend(clones_to_findings(f, &json));

        let json = match run_ra_query(&[
            "complexity",
            &abs_s,
            "--threshold",
            "15",
            "--format",
            "json",
        ]) {
            Ok(v) => v,
            Err(e) => return fail_closed("complexity_query_failed", &e),
        };
        findings.extend(complexity_to_findings(f, &json));

        let json =
            match run_ra_query(&["unwraps", &abs_s, "--severity", "high", "--format", "json"]) {
                Ok(v) => v,
                Err(e) => return fail_closed("unwraps_query_failed", &e),
            };
        findings.extend(unwraps_to_findings(f, &json));
    }

    // Callers fence (Y.4): for each new pub item introduced by the diff,
    // ask ra-query who calls it. Zero callers in workspace is a Blocker
    // (split-brain candidate). Pre-diff worktree creation failure and any
    // ra-query failure inside the fence are fail-closed (Y.5). Cleanup
    // after the fence is best-effort and does not override findings.
    let pre = match create_pre_diff_worktree(workspace, base_branch) {
        Ok(p) => p,
        Err(e) => return fail_closed("git_worktree_failed", &format!("git worktree add: {e}")),
    };
    let callers_result: Result<Vec<ReviewFinding>, (&'static str, String)> = (|| {
        let mut out = Vec::new();
        for f in &changed {
            let abs = workspace.join(f);
            if !abs.is_file() {
                continue;
            }
            let post = pub_surface_at(workspace, f);
            let pre_items = pub_surface_at(&pre, f);
            let new_items = match classify_new_pub_items(f, post, pre_items) {
                Ok(items) => items,
                Err(meta) => return Err(("pub_surface_unresolved", meta.message.clone())),
            };
            for item in new_items {
                let abs_s = workspace.join(f).to_string_lossy().into_owned();
                let pos = format!("{}:{}:{}", abs_s, item.line, item.col);
                match callers_at(workspace, &pos, item.col) {
                    Ok(Some(0)) => out.push(callers_zero_finding(f, &item)),
                    Ok(Some(_)) => {}
                    Ok(None) => out.push(callers_unresolved_finding(f, &item, &pos)),
                    Err(e) => return Err(("callers_query_failed", format!("{pos}: {e}"))),
                }
            }
        }
        Ok(out)
    })();
    cleanup_pre_diff_worktree(&pre);
    match callers_result {
        Ok(more) => {
            findings.extend(more);
            findings
        }
        Err((reason, detail)) => fail_closed(reason, &detail),
    }
}

/// Decide how to react to a (post, pre) pub-surface pair for one file.
/// On either-side failure, return the meta-finding the caller must emit
/// instead of running difference/callers — v3 regression: pre-side failure
/// must not cascade into N callers_zero false-positives on pre-existing
/// pub items.
fn classify_new_pub_items(
    file: &str,
    post: Result<Vec<PubItem>, String>,
    pre: Result<Vec<PubItem>, String>,
) -> Result<Vec<PubItem>, Box<ReviewFinding>> {
    match (post, pre) {
        (Err(e), _) => Err(Box::new(pub_surface_unresolved_finding(file, "post", &e))),
        (_, Err(e)) => Err(Box::new(pub_surface_unresolved_finding(file, "pre", &e))),
        (Ok(post_items), Ok(pre_items)) => Ok(difference(&post_items, &pre_items)),
    }
}

fn changed_rs_files_via_git(workspace: &Path, base_branch: &str) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg(format!("origin/{base_branch}...HEAD"))
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git diff: {e}"))?;
    if !out.status.success() {
        return Err(format!("git diff exit {:?}", out.status.code()));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(s.lines()
        .filter(|line| {
            line.ends_with(".rs") && !line.starts_with("tests/") && !line.contains("/tests/")
        })
        .map(|l| l.to_string())
        .collect())
}

fn fence_severity(kind: FenceKind) -> Severity {
    FENCE_MATRIX
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map(|(_, _, s)| s.clone())
        .unwrap_or(Severity::Blocker)
}

fn mechanical_fence_finding(
    file: &str,
    line: Option<u32>,
    rule: &str,
    kind: FenceKind,
    message: String,
) -> ReviewFinding {
    ReviewFinding {
        file: Some(file.to_string()),
        line,
        severity: fence_severity(kind),
        origin: FindingOrigin::Mechanical {
            tool: "ra-query".into(),
            rule: Some(rule.into()),
        },
        category: "fence".into(),
        message,
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

/// Fold a `ra-query clones --format json` response into Mechanical findings.
/// Emits one `clones_in_loop` finding per function with `clones_in_loop > 0`,
/// and one `clone_prod` finding per function with non-loop, non-Arc/Rc clones.
pub fn clones_to_findings(file: &str, json: &Value) -> Vec<ReviewFinding> {
    let mut out = Vec::new();
    let functions = match json.get("functions").and_then(Value::as_array) {
        Some(a) => a,
        None => return out,
    };
    for fn_v in functions {
        let name = fn_v.get("name").and_then(Value::as_str).unwrap_or("");
        let line = fn_v.get("line").and_then(Value::as_u64).map(|n| n as u32);
        let clones_in_loop = fn_v
            .get("clones_in_loop")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let clone_calls = fn_v.get("clone_calls").and_then(Value::as_u64).unwrap_or(0);
        let arc_rc = fn_v
            .get("arc_rc_pattern")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if clones_in_loop > 0 {
            out.push(mechanical_fence_finding(
                file,
                line,
                "clones_in_loop",
                FenceKind::ClonesInLoop,
                format!("{name}: {clones_in_loop} clone call(s) inside loop"),
            ));
        }
        let prod_clones = clone_calls
            .saturating_sub(clones_in_loop)
            .saturating_sub(arc_rc);
        if prod_clones > 0 {
            out.push(mechanical_fence_finding(
                file,
                line,
                "clone_prod",
                FenceKind::CloneProd,
                format!("{name}: {prod_clones} non-Arc/Rc clone call(s) in production code"),
            ));
        }
    }
    out
}

/// Fold a `ra-query complexity --threshold 15 --format json` response into
/// Mechanical findings. The CLI flag does the threshold filtering, so every
/// function present in the output is over-budget.
pub fn complexity_to_findings(file: &str, json: &Value) -> Vec<ReviewFinding> {
    let mut out = Vec::new();
    let functions = match json.get("functions").and_then(Value::as_array) {
        Some(a) => a,
        None => return out,
    };
    for fn_v in functions {
        let name = fn_v.get("name").and_then(Value::as_str).unwrap_or("");
        let line = fn_v.get("line").and_then(Value::as_u64).map(|n| n as u32);
        let cognitive = fn_v.get("cognitive").and_then(Value::as_u64).unwrap_or(0);
        out.push(mechanical_fence_finding(
            file,
            line,
            "complexity",
            FenceKind::Complexity,
            format!("{name}: cognitive complexity {cognitive} exceeds threshold"),
        ));
    }
    out
}

// ---------- callers fence (Y.4) ----------

/// One pub item from `ra-query pub-surface`, augmented with the column where
/// `name` appears on `line` (the position-form anchor `ra-query callers`
/// requires). `col == 0` means column resolution failed — the caller emits
/// a visible `callers_unresolved` finding rather than silently skipping.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PubItem {
    name: String,
    kind: String,
    file: String,
    line: usize,
    col: usize,
}

fn create_pre_diff_worktree(workspace: &Path, base_branch: &str) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path =
        std::env::temp_dir().join(format!("agentry-pre-diff-{}-{}", std::process::id(), nanos,));
    let out = Command::new("git")
        .arg("worktree")
        .arg("add")
        .arg("--detach")
        .arg(&path)
        .arg(format!("origin/{base_branch}"))
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git worktree add: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(path)
}

fn cleanup_pre_diff_worktree(path: &Path) {
    let _ = Command::new("git")
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn find_crate_root(file_abs: &Path) -> Option<PathBuf> {
    let mut current = file_abs.parent()?;
    loop {
        if current.join("Cargo.toml").is_file() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn column_for_name_at_line(file_abs: &Path, name: &str, line: usize) -> Option<usize> {
    let content = std::fs::read_to_string(file_abs).ok()?;
    let target = content.lines().nth(line.saturating_sub(1))?;
    let off = target.find(name)?;
    Some(off + 1)
}

fn pub_surface_at(dir: &Path, file: &str) -> Result<Vec<PubItem>, String> {
    let abs = dir.join(file);
    // A file absent from this side (e.g. new file in post that didn't exist
    // in pre) is a legitimate "no pub items here" — not a failure. Real
    // failures (missing crate root, ra-query crash) bubble up below.
    if !abs.is_file() {
        return Ok(Vec::new());
    }
    let crate_root = find_crate_root(&abs)
        .ok_or_else(|| format!("no Cargo.toml found walking up from {}", abs.display()))?;
    let crate_root_s = crate_root.to_string_lossy().into_owned();
    let json = run_ra_query(&["pub-surface", &crate_root_s])
        .map_err(|e| format!("ra-query pub-surface {}: {e}", crate_root_s))?;
    let arr = json
        .as_array()
        .ok_or_else(|| "ra-query pub-surface response was not a JSON array".to_string())?;
    let abs_canon = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    Ok(arr
        .iter()
        .filter_map(|v| {
            let item_file = v.get("file")?.as_str()?;
            let item_path = Path::new(item_file)
                .canonicalize()
                .unwrap_or_else(|_| Path::new(item_file).to_path_buf());
            if item_path != abs_canon {
                return None;
            }
            let name = v.get("name")?.as_str()?.to_string();
            let kind = v.get("kind")?.as_str()?.to_string();
            let line = v.get("line")?.as_u64()? as usize;
            let col = column_for_name_at_line(&abs, &name, line).unwrap_or(0);
            Some(PubItem {
                name,
                kind,
                file: file.to_string(),
                line,
                col,
            })
        })
        .collect())
}

fn callers_at(workspace: &Path, pos: &str, col: usize) -> Result<Option<usize>, String> {
    if col == 0 {
        return Ok(None);
    }
    let workspace_s = workspace.to_string_lossy().into_owned();
    let json = run_ra_query(&["callers", pos, "-p", &workspace_s, "-f", "json"])?;
    if let Some(arr) = json.get("callers").and_then(Value::as_array) {
        Ok(Some(arr.len()))
    } else {
        Ok(json.as_array().map(|arr| arr.len()))
    }
}

fn difference(post: &[PubItem], pre: &[PubItem]) -> Vec<PubItem> {
    post.iter()
        .filter(|p| {
            !pre.iter()
                .any(|q| q.name == p.name && q.kind == p.kind && q.file == p.file)
        })
        .cloned()
        .collect()
}

fn callers_zero_finding(file: &str, item: &PubItem) -> ReviewFinding {
    ReviewFinding {
        file: Some(file.to_string()),
        line: Some(item.line as u32),
        severity: fence_severity(FenceKind::CallersZero),
        origin: FindingOrigin::Mechanical {
            tool: "ra-query".into(),
            rule: Some("callers_zero".into()),
        },
        category: "fence".into(),
        message: format!(
            "split-brain candidate: new pub item `{}` has zero callers in workspace",
            item.name,
        ),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

fn pub_surface_unresolved_finding(file: &str, side: &str, detail: &str) -> ReviewFinding {
    ReviewFinding {
        file: Some(file.to_string()),
        line: None,
        severity: Severity::Warn,
        origin: FindingOrigin::Mechanical {
            tool: "ra-query".into(),
            rule: Some("pub_surface_unresolved".into()),
        },
        category: "fence".into(),
        message: format!(
            "callers fence skipped for `{file}`: {side}-diff pub-surface unresolved ({detail}) — please verify integration manually",
        ),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

fn callers_unresolved_finding(file: &str, item: &PubItem, pos: &str) -> ReviewFinding {
    ReviewFinding {
        file: Some(file.to_string()),
        line: Some(item.line as u32),
        severity: Severity::Warn,
        origin: FindingOrigin::Mechanical {
            tool: "ra-query".into(),
            rule: Some("callers_unresolved".into()),
        },
        category: "fence".into(),
        message: format!(
            "callers query could not resolve `{}` at {pos} — please verify integration manually",
            item.name,
        ),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

/// Fold a `ra-query unwraps --severity high --format json` response into
/// Mechanical findings. The CLI flag does severity gating; every function
/// present in the output has at least one high-or-critical unwrap/expect.
pub fn unwraps_to_findings(file: &str, json: &Value) -> Vec<ReviewFinding> {
    let mut out = Vec::new();
    let functions = match json.get("functions").and_then(Value::as_array) {
        Some(a) => a,
        None => return out,
    };
    for fn_v in functions {
        let name = fn_v.get("name").and_then(Value::as_str).unwrap_or("");
        let line = fn_v.get("line").and_then(Value::as_u64).map(|n| n as u32);
        let total = fn_v.get("total").and_then(Value::as_u64).unwrap_or(0);
        out.push(mechanical_fence_finding(
            file,
            line,
            "unwraps",
            FenceKind::Unwraps,
            format!("{name}: {total} high-severity unwrap/expect call(s)"),
        ));
    }
    out
}

// ---------- reviewer prompt builder (extracted from reviewer_claude_runner, brief X.7c) ----------

const REVIEW_ISSUE_BODY_BUDGET: usize = 2000;

/// Build the strict reviewer prompt — verbatim from
/// `REVIEWER_CLAUDE_AGENTRY_SCRIPT`, including the strict output-format
/// guidance, scope guardrail, verb-completeness check, and the four
/// CRITICAL audits (role-spec, bootstrap-command, daemon-lifecycle,
/// state-machine idempotency). Any prose drift here changes reviewer
/// behaviour mid-port.
pub fn build_review_prompt(
    base_branch: &str,
    issue_title: &str,
    issue_body: &str,
    diff_text: &str,
) -> String {
    let issue_body_head = head_bytes(issue_body, REVIEW_ISSUE_BODY_BUDGET);
    let mut s = String::new();
    s.push_str(&format!(
        "You are a senior code reviewer for the agentry project — a Rust workspace\n\
         that orchestrates short-lived agent containers. Review the following diff\n\
         produced against branch \"{base_branch}\" in response to this task:\n\
         \n\
         TITLE: {issue_title}\n\
         \n\
         BODY (first 2000 chars):\n\
         {issue_body_head}\n\
         \n\
         --- DIFF ---\n\
         {diff_text}\n\
         --- END DIFF ---\n"
    ));
    s.push_str(
        "\nOutput EXACTLY a JSON array of findings — nothing else. No markdown fences,\n\
         no prose, no preamble, no explanation. Each element:\n\
         \n\
         {\n\
           \"severity\": \"blocker\" | \"warn\",\n\
           \"category\": \"design\" | \"naming\" | \"clarity\" | \"invariant\" | \"other\",\n\
           \"message\": \"one-sentence human-readable description (max 200 chars)\",\n\
           \"prohibitions\": [\"...\"],   // for blockers, what the rework must NOT do\n\
           \"requirements\": [\"...\"]    // for blockers, what the rework MUST do\n\
         }\n\
         \n\
         Guidance:\n\
         - `blocker` = ships-broken, security-risk, invariant-violation, wrong abstraction.\n\
         - `warn` = minor cleanup, non-load-bearing style.\n\
         - If the diff is acceptable as-is, output exactly: []\n\
         - Maximum 6 findings. Prefer a single Blocker over many Warns.\n\
         - Do not comment on fmt/clippy/test — those are mechanical-reviewer scope.\n\
         - For each Blocker, populate `prohibitions` (things the next coder pass\n\
           MUST NOT do) and `requirements` (things the next coder pass MUST do).\n\
           These anchor the rework so the coder does not solve the wrong problem.\n\
         - For Warns, both arrays SHOULD be empty — a warn is informational, not\n\
           a rework constraint.\n\
         \n\
         Scope guardrail (CRITICAL):\n\
         - ONLY flag changes INSIDE the DIFF. Pre-existing inconsistencies in the\n\
           repo that the diff did not touch are OUT of scope for blocker-level\n\
           findings.\n\
         - If you notice a pre-existing concern adjacent to the diff, you MAY emit\n\
           at most ONE warn-level finding with category \"scope\" describing it, so\n\
           it is on-record for a follow-up brief — but NEVER emit a blocker for\n\
           something the diff did not change.\n\
         - The unit of review is \"did THIS diff ship broken/unsafe/wrong?\", not\n\
           \"is the whole repo now consistent?\".\n\
         \n\
         Verb-completeness check (CRITICAL):\n\
         - The TASK BODY above may contain explicit verbs: CREATE, UPDATE, REPLACE,\n\
           DELETE, MOVE — usually headed as \"### N. <VERB> <file:line>\" or the bare\n\
           form \"<VERB> <file:line>\".\n\
         - For EACH such verb in the body, verify the diff contains the corresponding\n\
           change at the named location (file path and approximate area).\n\
         - An unapplied verb is a blocker with category \"invariant\" and message\n\
           \"unapplied verb: <short description of what was missed>\".\n\
         - If the body contains no verb syntax (legacy free-form brief), skip this\n\
           check and apply only the design/naming/clarity/invariant guidance above.\n\
         - Applied-but-incomplete counts as unapplied (e.g. the verb asked to change\n\
           three sites and only two were touched — the remaining one is unapplied).\n\
         \n\
         Role-spec audit (CRITICAL):\n\
         - If the diff adds or modifies an `AgentRole` (i.e. introduces a `RoleName(...)` registration in seed.rs or changes the fields of an existing one), verify each of:\n\
           (a) `permit_scope` is minimal for the stated job — no fs:write outside the workspace, no net access unless justified, no git tools on roles that do not ship code.\n\
           (b) `tool_allowlist` matches what the role's entrypoint actually does (a read-only role must not be allowed to write arbitrary streams).\n\
           (c) the deny-list is explicit for the categories of tool the role does not need.\n\
           (d) any `binaries` or `mcp_servers` named are justified by the role's job.\n\
         - Mismatches are blockers with category \"invariant\" and a message starting with \"role-spec audit:\".\n\
         - This complements (does not replace) the scope-guardrail and verb-completeness checks above.\n\
         \n\
         Bootstrap-command audit (CRITICAL):\n\
         - If the diff modifies any role's `extra_bootstrap` shell strings, verify each shell command:\n\
           (a) `cargo install --git URL --bin <name>` is rejected when the target is a workspace with multiple binaries — must use positional package name (e.g. `cfdb-cli`, `application`) or `--package`.\n\
           (b) Bootstrap commands that may transiently fail must end with `|| true` for fault tolerance, matching the existing `reviewer-mechanical-agentry` quality-hygiene install pattern.\n\
           Mismatch is a blocker with category \"invariant\".\n\
         \n\
         Daemon-lifecycle ordering (CRITICAL):\n\
         - If the diff modifies the daemon's `handle_brief` shipping flow (workspace teardown, chain-trigger, terminal-handler), verify the ORDER:\n\
           chain-trigger MUST read `next_brief_refs` and submit children to Redis BEFORE workspace destruction.\n\
           Reason: planner-emitted child JSONs live IN the workspace; destroyed-before-read = lost children.\n\
           Wrong order is a blocker with category \"invariant\".\n\
         \n\
         State-machine emission idempotency (CRITICAL):\n\
         - If the diff adds or modifies a state-machine compose/finalize function (DOL `compose_meta_verdict`, future composers in recursive sub-planning, etc.), verify exactly-once semantics:\n\
           guard the emission with SETNX on a Redis marker key, OR a Redis transaction, OR an equivalent atomic check.\n\
           Concurrent terminal handlers can re-enter; without the gate, duplicate verdicts will fire (observed in A7v3: 3× duplicate failed-verdicts for one meta-brief).\n\
           Missing idempotency gate is a blocker with category \"invariant\".\n\
         \n\
         Your response, right now, starting with [ and ending with ]:\n",
    );
    s
}

// ---------- ra-query pre-pass (Phase 2.2 #87) ----------

/// Outcome of [`ra_query_pre_pass`]. Carries Warn-severity informational
/// findings (`unwraps`/`complexity`/`callers`) on success, or a
/// `skipped_reason` when the pre-pass could not run (binary missing,
/// ra-query call failed). Skip is non-blocking — the runner emits a
/// `ra-query pre-pass skipped` event and continues to LLM-only review.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RaQueryPrePass {
    pub findings: Vec<ReviewFinding>,
    pub skipped_reason: Option<String>,
}

/// Pre-pass parser for `ra-query unwraps --severity high --format json`.
/// Mirrors [`unwraps_to_findings`] but emits Warn-severity findings tagged
/// `category = "unwraps"` so the LLM-prompt summary distinguishes them.
pub fn prepass_unwraps_to_findings(file: &str, json: &Value) -> Vec<ReviewFinding> {
    let mut out = unwraps_to_findings(file, json);
    for f in &mut out {
        f.severity = Severity::Warn;
        f.category = "unwraps".into();
    }
    out
}

/// Pre-pass parser for `ra-query complexity --threshold 15 --format json`.
/// Emits Warn-severity findings tagged `category = "complexity"`.
pub fn prepass_complexity_to_findings(file: &str, json: &Value) -> Vec<ReviewFinding> {
    let mut out = complexity_to_findings(file, json);
    for f in &mut out {
        f.severity = Severity::Warn;
        f.category = "complexity".into();
    }
    out
}

/// Pre-pass parser for one `ra-query callers <pos> -f json` response. Emits
/// a single zero-callers Warn finding when the response lists no callers,
/// or an empty vec otherwise. Accepts either `{"callers": [...]}` or a bare
/// JSON array — same shapes the [`run_fence`] callers helper handles.
pub fn prepass_callers_to_findings(
    file: &str,
    item_name: &str,
    line: u32,
    json: &Value,
) -> Vec<ReviewFinding> {
    let count = json
        .get("callers")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .or_else(|| json.as_array().map(|a| a.len()))
        .unwrap_or(0);
    if count != 0 {
        return Vec::new();
    }
    vec![ReviewFinding {
        file: Some(file.to_string()),
        line: Some(line),
        severity: Severity::Warn,
        origin: FindingOrigin::Mechanical {
            tool: "ra-query".into(),
            rule: Some("callers_zero".into()),
        },
        category: "callers".into(),
        message: format!("pub item `{item_name}` has zero callers in workspace"),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }]
}

/// Format pre-pass findings as a multi-line summary for the reviewer
/// prompt — one line per finding shaped `severity:category:file:line:message`.
/// Returns the literal string `(none)` for an empty slice so the prompt
/// section has a stable shape regardless of pre-pass outcome.
pub fn format_mechanical_findings_summary(findings: &[ReviewFinding]) -> String {
    if findings.is_empty() {
        return "(none)".into();
    }
    findings
        .iter()
        .map(|f| {
            let sev = match f.severity {
                Severity::Blocker => "blocker",
                Severity::Warn => "warn",
            };
            let line = f.line.map(|l| l.to_string()).unwrap_or_default();
            let file = f.file.as_deref().unwrap_or("");
            format!("{sev}:{}:{file}:{line}:{}", f.category, f.message)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the strict reviewer prompt with an extra "mechanical findings"
/// section appended. The `mechanical_findings_summary` is the output of
/// [`format_mechanical_findings_summary`] (or `(none)` when the pre-pass
/// found nothing or was skipped). The base prompt body is produced by
/// [`build_review_prompt`] verbatim — the section is purely additive so
/// LLM-only review (skipped pre-pass) reads identically aside from a
/// `(none)` summary.
pub fn build_review_prompt_with_mechanical_findings(
    base_branch: &str,
    issue_title: &str,
    issue_body: &str,
    diff_text: &str,
    mechanical_findings_summary: &str,
) -> String {
    let mut s = build_review_prompt(base_branch, issue_title, issue_body, diff_text);
    s.push_str(&format!(
        "\nMechanical findings already detected (do not re-flag, but consider them in your architectural review):\n{mechanical_findings_summary}\n",
    ));
    s
}

/// Run the ra-query pre-pass against `workspace`'s diff vs `base_branch`.
/// For each changed `*.rs` file outside `tests/`, calls `ra-query unwraps`,
/// `ra-query complexity`, and (per pub item from `ra-query pub-surface`)
/// `ra-query callers`, folding the results into Warn-severity findings.
///
/// Skip-friendly (Phase 2.2): missing binary or any ra-query failure
/// returns a [`RaQueryPrePass`] with `skipped_reason` populated and an
/// empty `findings` vec — the caller emits `{"msg":"ra-query pre-pass
/// skipped","reason":"<short>"}` and continues with LLM-only review. This
/// is intentionally distinct from the fail-closed [`run_fence`] which
/// blocks on substrate failure: the pre-pass is informational, the fence
/// is gatekeeping.
pub fn ra_query_pre_pass(workspace: &Path, base_branch: &str) -> RaQueryPrePass {
    if !ra_query_present() {
        return RaQueryPrePass {
            findings: Vec::new(),
            skipped_reason: Some("ra-query binary missing".into()),
        };
    }
    let changed = match prepass_changed_rs_files(workspace, base_branch) {
        Ok(v) => v,
        Err(e) => {
            return RaQueryPrePass {
                findings: Vec::new(),
                skipped_reason: Some(format!("git diff failed: {e}")),
            };
        }
    };
    let workspace_s = workspace.to_string_lossy().into_owned();
    let mut findings = Vec::new();
    for f in &changed {
        let abs = workspace.join(f);
        if !abs.is_file() {
            continue;
        }
        let abs_s = abs.to_string_lossy().into_owned();

        match run_ra_query(&["unwraps", &abs_s, "--severity", "high", "--format", "json"]) {
            Ok(j) => findings.extend(prepass_unwraps_to_findings(f, &j)),
            Err(e) => {
                return RaQueryPrePass {
                    findings: Vec::new(),
                    skipped_reason: Some(format!("ra-query unwraps failed: {e}")),
                };
            }
        }

        match run_ra_query(&[
            "complexity",
            &abs_s,
            "--threshold",
            "15",
            "--format",
            "json",
        ]) {
            Ok(j) => findings.extend(prepass_complexity_to_findings(f, &j)),
            Err(e) => {
                return RaQueryPrePass {
                    findings: Vec::new(),
                    skipped_reason: Some(format!("ra-query complexity failed: {e}")),
                };
            }
        }

        let items = match pub_surface_at(workspace, f) {
            Ok(items) => items,
            Err(e) => {
                return RaQueryPrePass {
                    findings: Vec::new(),
                    skipped_reason: Some(format!("ra-query pub-surface failed: {e}")),
                };
            }
        };
        for item in items {
            if item.col == 0 {
                continue;
            }
            let pos = format!("{}:{}:{}", abs_s, item.line, item.col);
            match run_ra_query(&["callers", &pos, "-p", &workspace_s, "-f", "json"]) {
                Ok(j) => findings.extend(prepass_callers_to_findings(
                    f,
                    &item.name,
                    item.line as u32,
                    &j,
                )),
                Err(e) => {
                    return RaQueryPrePass {
                        findings: Vec::new(),
                        skipped_reason: Some(format!("ra-query callers failed: {e}")),
                    };
                }
            }
        }
    }
    RaQueryPrePass {
        findings,
        skipped_reason: None,
    }
}

fn prepass_changed_rs_files(workspace: &Path, base_branch: &str) -> Result<Vec<String>, String> {
    let range = format!("{base_branch}...HEAD");
    let out = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg(&range)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git diff: {e}"))?;
    if !out.status.success() {
        return Err(format!("git diff exit {:?}", out.status.code()));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(s.lines()
        .filter(|line| {
            line.ends_with(".rs") && !line.starts_with("tests/") && !line.contains("/tests/")
        })
        .map(|l| l.to_string())
        .collect())
}

// ---------- coder-binary helpers (extracted from coder_claude_runner, brief X.7c) ----------

/// Issue-body byte budget used by [`build_self_review_prompt`].
pub const SELF_REVIEW_ISSUE_BODY_BUDGET: usize = 3000;

/// Prior-iteration blocker finding extracted from the bundle's
/// `team_context.messages[].payload.findings[]`. Carries just enough to
/// rebuild the rework banner and stamp self-review findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorFinding {
    pub message: String,
    pub prohibitions: Vec<String>,
    pub requirements: Vec<String>,
}

/// Parsed brief-bundle context as consumed by the coder runner.
#[derive(Debug)]
pub struct BriefContext {
    pub brief_id: String,
    pub base_branch: String,
    pub issue_title: String,
    pub issue_body: String,
    pub acceptance: String,
    pub branch: String,
    pub topology_name: String,
    pub rework_banner: String,
    pub blocker_findings: Vec<PriorFinding>,
    pub allowed_tools: Vec<String>,
}

/// One unapplied verb surfaced by self-review. `applied_form` and
/// `rationale` are optional (empty string ⇒ unset) so legacy coders that
/// emit only a string verb description still parse.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnappliedVerb {
    pub verb: String,
    #[serde(default)]
    pub applied_form: String,
    #[serde(default)]
    pub rationale: String,
}

impl UnappliedVerb {
    pub fn applied_form_or_dash(&self) -> &str {
        if self.applied_form.is_empty() {
            "—"
        } else {
            &self.applied_form
        }
    }
    pub fn rationale_or_dash(&self) -> &str {
        if self.rationale.is_empty() {
            "—"
        } else {
            &self.rationale
        }
    }
}

/// Parsed self-review JSON object (`{all_applied, unapplied}`).
#[derive(Debug, PartialEq, Eq)]
pub struct SelfReviewResult {
    pub all_applied: bool,
    pub unapplied: Vec<UnappliedVerb>,
}

/// Build a [`BriefContext`] from a startup bundle JSON value.
pub fn parse_brief_context(bundle: &Value) -> BriefContext {
    let brief_id = pointer_str(bundle, "/brief/id").to_string();
    let base_branch = pointer_str_or(bundle, "/brief/payload/base_branch", "develop");
    let issue_title = pointer_str(bundle, "/brief/payload/issue_title").to_string();
    let issue_body = pointer_str(bundle, "/brief/payload/issue_body").to_string();
    let acceptance = pointer_str_or(bundle, "/brief/payload/acceptance", "true");
    let topology_name = pointer_str(bundle, "/brief/topology/name").to_string();
    let blocker_findings = collect_blocker_findings(bundle);
    let rework_banner = if blocker_findings.is_empty() {
        String::new()
    } else {
        build_rework_banner(&blocker_findings)
    };
    let branch = format!("auto/{brief_id}");
    let allowed_tools = parse_allowed_tools(bundle);
    BriefContext {
        brief_id,
        base_branch,
        issue_title,
        issue_body,
        acceptance,
        branch,
        topology_name,
        rework_banner,
        blocker_findings,
        allowed_tools,
    }
}

/// Walk `team_context.messages[].payload.findings[]` and collect all
/// `severity == "blocker"` entries.
pub fn collect_blocker_findings(bundle: &Value) -> Vec<PriorFinding> {
    let messages = bundle
        .pointer("/team_context/messages")
        .and_then(Value::as_array);
    let Some(messages) = messages else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in messages {
        let findings = m.pointer("/payload/findings").and_then(Value::as_array);
        let Some(findings) = findings else { continue };
        for f in findings {
            if f.get("severity").and_then(Value::as_str) != Some("blocker") {
                continue;
            }
            out.push(PriorFinding {
                message: f
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                prohibitions: string_array_field(f, "prohibitions"),
                requirements: string_array_field(f, "requirements"),
            });
        }
    }
    out
}

/// Compose the rework banner injected into the coder prompt when the
/// reviewer flagged blockers on a prior iteration.
pub fn build_rework_banner(findings: &[PriorFinding]) -> String {
    let mut feedback_block = String::new();
    for (i, f) in findings.iter().enumerate() {
        if i > 0 {
            feedback_block.push('\n');
        }
        feedback_block.push_str(&format!(
            "- BLOCKER: {}\n  Prohibitions: {}\n  Requirements: {}",
            f.message,
            f.prohibitions.join("; "),
            f.requirements.join("; "),
        ));
    }
    format!(
        "**This is a REWORK iteration.**\n\
         \n\
         A prior coder pass on this brief shipped a commit that is already on HEAD of this worktree. \
         The reviewer flagged the following BLOCKER findings against that commit. Read the existing \
         diff with `git diff ${{base_branch}}...HEAD`, identify the sites the findings name, and edit \
         those sites to satisfy each requirement and avoid each prohibition. Do NOT replan from \
         scratch and do NOT recreate files that already exist.\n\
         \n\
         --- Prior reviewer findings ---\n\
         {feedback_block}\n\
         --- End findings ---"
    )
}

/// Build the coder-claude prompt — verb-structured task description,
/// branch context, acceptance command, mid-session validation guidance.
pub fn build_coder_prompt(
    base_branch: &str,
    branch: &str,
    rework_banner: &str,
    issue_title: &str,
    issue_body: &str,
    acceptance: &str,
) -> String {
    format!(
        "You are the coder role inside the agentry autonomous team, operating in the\n\
         container-local working directory /workspace. The repo is cloned at\n\
         branch \"{base_branch}\"; you are on a fresh branch \"{branch}\".\n\
         \n\
         Your task is described in verb-structured form below. Follow it literally:\n\
         each verb (CREATE / UPDATE / REPLACE / DELETE / MOVE) names a transformation\n\
         on a specific file:line target. Do NOT invent additional changes.\n\
         \n\
         # /usr/local/bin/ship — runs the brief.kind's validator pipeline against /workspace and prints a JSON report. Calling it is OPTIONAL in this brief; brief 6 makes it the only path to publication. Use it as a self-check before git commit if you want.\n\
         \n\
         {rework_banner}\n\
         \n\
         Task title: {issue_title}\n\
         \n\
         Task body:\n\
         {issue_body}\n\
         \n\
         Constraints:\n\
         - Operate only inside /workspace. Never touch files outside it.\n\
         - When you are done editing, the acceptance command below must pass. You\n  \
           may run it yourself to check. The orchestrator will re-run it before\n  \
           accepting the diff:\n    \
           {acceptance}\n\
         - Do not commit or push. The orchestrator handles commit and push on your\n  \
           behalf after you exit.\n\
         - The orchestrator may be running you in `agentry-self-host-v1` topology (or a later v1+). In that case: do not commit, do not push. The `/usr/local/bin/ship` tool (when called) runs the validator pipeline against your changes; if it returns ok, exit and the orchestrator's git-operator role takes over. Topology name is in $topology_name.\n\
         - For mid-session validation, invoke `quality-fast` (no args, scoped to changed\n  \
           files by default). Do NOT invoke cargo for navigation or speculative\n  \
           checks; cargo is reserved for `cargo fmt`. The substrate runs full\n  \
           validators after you exit.\n\
         \n\
         When the transformations are complete and the acceptance passes, simply\n\
         report success and exit.\n"
    )
}

/// Bash regex `-v[1-9][0-9]*$` — true for `agentry-self-host-v1`,
/// `agentry-self-host-v12`, etc. False for v0 (the v0 topology runs the
/// local commit/push exitpoint path).
pub fn is_v1_plus_topology(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 3 {
        return false;
    }
    let mut digit_start = bytes.len();
    while digit_start > 0 && bytes[digit_start - 1].is_ascii_digit() {
        digit_start -= 1;
    }
    if digit_start == bytes.len() {
        return false;
    }
    if digit_start < 2 || bytes[digit_start - 1] != b'v' || bytes[digit_start - 2] != b'-' {
        return false;
    }
    let digits = &bytes[digit_start..];
    let first = digits[0];
    (b'1'..=b'9').contains(&first) && digits[1..].iter().all(|b| b.is_ascii_digit())
}

/// Bash: `grep -qE '^(### [0-9]+\. |CREATE |UPDATE |REPLACE |DELETE |MOVE )'`.
/// True when the issue body contains explicit verb syntax somewhere on a
/// line — bare `CREATE foo`, `### 12. UPDATE foo`, etc.
pub fn body_has_verb_syntax(body: &str) -> bool {
    body.lines().any(line_starts_with_verb)
}

fn line_starts_with_verb(line: &str) -> bool {
    if line.starts_with("CREATE ")
        || line.starts_with("UPDATE ")
        || line.starts_with("REPLACE ")
        || line.starts_with("DELETE ")
        || line.starts_with("MOVE ")
    {
        return true;
    }
    if let Some(rest) = line.strip_prefix("### ") {
        let mut digit_count = 0usize;
        for c in rest.chars() {
            if c.is_ascii_digit() {
                digit_count += 1;
            } else {
                break;
            }
        }
        if digit_count == 0 {
            return false;
        }
        let after_digits = &rest[digit_count..];
        return after_digits.starts_with(". ");
    }
    false
}

/// Build the self-review prompt asking claude to verify each verb in the
/// body has a matching change in the staged diff.
pub fn build_self_review_prompt(issue_body: &str, staged_diff: &str) -> String {
    let body_head = head_bytes(issue_body, SELF_REVIEW_ISSUE_BODY_BUDGET);
    format!(
        "You are a self-review check for the agentry project. Below is the TASK\n\
         BODY (with explicit verbs — CREATE/UPDATE/REPLACE/DELETE/MOVE) and the\n\
         STAGED DIFF you are about to commit.\n\
         \n\
         TASK BODY:\n\
         {body_head}\n\
         \n\
         STAGED DIFF:\n\
         {staged_diff}\n\
         \n\
         For each verb declared in the task body, check whether it has been applied\n\
         in the diff at the named location. Output EXACTLY a JSON object — no markdown fences, no prose:\n\
         \n\
         {{\n\
         \x20\x20\"all_applied\": true,\n\
         \x20\x20\"unapplied\": []\n\
         }}\n\
         \n\
         When all verbs were applied as specified, set all_applied:true and leave\n\
         unapplied empty. When a verb was NOT applied, OR was applied with a\n\
         deliberate variant (different file content, different naming, different\n\
         structure than the brief literally specified), include it in unapplied\n\
         as an object:\n\
         \n\
         {{\n\
         \x20\x20\"verb\": \"<short verb description, max 200 chars>\",\n\
         \x20\x20\"applied_form\": \"<what was actually done in the diff, max 200 chars>\",\n\
         \x20\x20\"rationale\": \"<why the applied form differs, max 300 chars; empty when the verb was simply not done>\"\n\
         }}\n\
         \n\
         Maximum 6 entries. The rationale field is the most important: when you\n\
         deliberately deviated from the brief because it would conflict with project\n\
         convention or produce broken output, EXPLAIN why. The captain reads this\n\
         to decide accept/reject; an empty rationale signals an honest miss.\n\
         \n\
         Your response, right now, starting with {{ and ending with }}:\n",
    )
}

/// Parse the self-review claude reply into a [`SelfReviewResult`]. Strips
/// optional code fences and slices between the first `{` and last `}`.
/// Returns `None` when the reply does not contain a parseable JSON object.
pub fn parse_self_review_object(raw: &str) -> Option<SelfReviewResult> {
    let cleaned = strip_fences(raw);
    let sliced = slice_json_object(&cleaned)?;
    let v: Value = serde_json::from_str(sliced).ok()?;
    if !v.is_object() {
        return None;
    }
    let all_applied = v
        .get("all_applied")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let unapplied = v
        .get("unapplied")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| {
                    // Object form (new): {verb, applied_form, rationale}
                    if let Some(obj) = x.as_object() {
                        let verb = obj.get("verb").and_then(Value::as_str)?.to_string();
                        let applied_form = obj
                            .get("applied_form")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let rationale = obj
                            .get("rationale")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        Some(UnappliedVerb {
                            verb,
                            applied_form,
                            rationale,
                        })
                    // String form (legacy): just the verb description
                    } else {
                        x.as_str().map(|s| UnappliedVerb {
                            verb: s.to_string(),
                            applied_form: String::new(),
                            rationale: String::new(),
                        })
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Some(SelfReviewResult {
        all_applied,
        unapplied,
    })
}

/// Slice the substring between the first `{` and last `}` in `text`, or
/// `None` if either is missing or `}` precedes `{`.
pub fn slice_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&text[start..=end])
}

/// Build a Warn-severity mechanical-origin [`ReviewFinding`] (mirrors
/// [`mech_finding`] but at Warn severity). Used by the coder's
/// dead-pub-check phase to surface JSONL findings without blocking.
pub fn mech_finding_warn(tool: &str, category: &str, message: &str) -> ReviewFinding {
    ReviewFinding {
        file: None,
        line: None,
        severity: Severity::Warn,
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

// ---------- verifier-dol helpers (extracted from verifier_dol_runner, EPIC #161 Wave 3) ----------

/// Bash `tail -c 4096` — the last N bytes of the criterion's combined
/// stdout+stderr output that get attached to the `criterion passed` /
/// `criterion failed` event payload.
pub const CRITERION_OUTPUT_TAIL: usize = 4096;

/// Read the brief payload's `success_criteria` field. Returns `None`
/// when the field is missing, null, non-string, or the empty string —
/// matching the bash `jq -r '.brief.payload.success_criteria // ""'`
/// + `[ -z "$criterion" ]` check.
pub fn parse_success_criteria(bundle: &Value) -> Option<String> {
    let s = bundle
        .pointer("/brief/payload/success_criteria")
        .and_then(Value::as_str)?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Read the brief payload's `verifies_brief_id` field as a plain string.
/// Returns the empty string when missing / null / non-string — matching
/// the bash `jq -r '.brief.payload.verifies_brief_id // ""'` fallback.
pub fn parse_verifies_brief_id(bundle: &Value) -> String {
    bundle
        .pointer("/brief/payload/verifies_brief_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Map the criterion's exit code to a verdict: `0` ships, anything else
/// fails. Mirrors the bash `if bash -c "$criterion"; then ... else ...`
/// branching.
pub fn verdict_for_exit_code(code: i32) -> EventVerdict {
    if code == 0 {
        EventVerdict::Shipped
    } else {
        EventVerdict::Failed
    }
}

// ---------- preflight-criterion helpers (extracted from preflight_criterion_runner, EPIC #161 wave-bash) ----------

/// Tool name embedded in `FindingOrigin::Mechanical` for every
/// preflight-criterion smell finding. Matches the bash heredoc's
/// `emit_finding warn preflight-criterion criterion-quality "..."` call
/// site verbatim so the daemon-side projector / dashboard attribution
/// does not drift.
pub const PREFLIGHT_TOOL: &str = "preflight-criterion";

/// Category embedded in every preflight-criterion smell finding.
/// Matches the bash heredoc.
pub const PREFLIGHT_CATEGORY: &str = "criterion-quality";

/// Split a `success_criteria` string on the FIRST occurrence of `" : "`
/// (space-colon-space). Returns `(cmd, expected)` with the expected
/// portion trimmed. Returns `None` when the separator is absent.
///
/// Mirrors bash:
/// ```bash
/// case "$criterion" in
///     *' : '*) ;;
///     *) ... ;;
/// esac
/// cmd="${criterion%% : *}"
/// expected_raw="${criterion#* : }"
/// expected=$(printf '%s' "$expected_raw" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
/// ```
pub fn split_criterion(criterion: &str) -> Option<(String, String)> {
    let sep = " : ";
    let idx = criterion.find(sep)?;
    let cmd = criterion[..idx].to_string();
    let expected_raw = &criterion[idx + sep.len()..];
    let expected = expected_raw
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_string();
    Some((cmd, expected))
}

/// Smell 1 — `expected == "0"`, baseline numeric > 100, cmd contains
/// `wc -l`. The criterion looks like a count-zero filter but the
/// baseline is huge: the filter is almost certainly too broad and will
/// surface false positives.
///
/// Ports bash heredoc lines 912-920 from
/// `crates/orchestrator-runtime/src/seed.rs`:
/// ```bash
/// if printf '%s' "$expected" | grep -qE '^[0-9]+$' \
///     && printf '%s' "$baseline" | grep -qE '^[0-9]+$' \
///     && [ "$expected" = "0" ] \
///     && [ "$baseline" -gt 100 ] \
///     && printf '%s' "$cmd" | grep -qF 'wc -l'; then
///     emit_finding warn preflight-criterion criterion-quality \
///         "criterion baseline ($baseline) is far from expected ($expected) — likely false positives if filter is naive"
/// fi
/// ```
pub fn smell_huge_baseline_zero_expected(
    cmd: &str,
    baseline: &str,
    expected: &str,
) -> Option<ReviewFinding> {
    if expected != "0" {
        return None;
    }
    if !is_ascii_digits(expected) || !is_ascii_digits(baseline) {
        return None;
    }
    let baseline_n: u64 = baseline.parse().ok()?;
    if baseline_n <= 100 {
        return None;
    }
    if !cmd.contains("wc -l") {
        return None;
    }
    Some(preflight_warn_finding(format!(
        "criterion baseline ({baseline}) is far from expected ({expected}) — likely false positives if filter is naive"
    )))
}

/// Smell 2 — cmd contains the literal `grep -v 'mod tests'` filter.
/// Canonical broken pattern from #51: the filter strips lines that
/// literally contain `mod tests` but does not exclude `#[cfg(test)]`
/// scopes, so test code still leaks past the count.
///
/// Ports bash heredoc lines 922-926.
pub fn smell_grep_v_mod_tests(cmd: &str) -> Option<ReviewFinding> {
    if !cmd.contains("grep -v 'mod tests'") {
        return None;
    }
    Some(preflight_warn_finding(
        "grep -v 'mod tests' filters lines containing literal text but not #[cfg(test)] scopes; use a Rust-aware tool like ra-query or cfdb instead".to_string(),
    ))
}

/// Smell 3 — cmd contains `wc -l` AND does NOT contain `#[cfg(test)]`.
/// Counting lines without an explicit test-scope exclusion likely
/// includes test code in the baseline.
///
/// Ports bash heredoc lines 928-933.
pub fn smell_wc_l_without_cfg_test(cmd: &str) -> Option<ReviewFinding> {
    if !cmd.contains("wc -l") {
        return None;
    }
    if cmd.contains("#[cfg(test)]") {
        return None;
    }
    Some(preflight_warn_finding(
        "counting via wc -l without test-scope exclusion may include test code".to_string(),
    ))
}

/// `DoneReason.cause` discriminant emitted by preflight-criterion-runner
/// when one of the blocking smell heuristics (smell-1 or smell-2) fires.
/// The daemon-side trace translator (wired in 84b-2) folds this into
/// `BriefEvent::PreflightSmellDetected`, which the FSM transitions into
/// `BriefState::Failed { reason: Reason::PreflightSmell }`. Smell-3 stays
/// Warn-only and does NOT emit this cause.
pub const PREFLIGHT_SMELL_CAUSE: &str = "preflight_smell";

/// Returns the first blocking preflight-criterion smell finding for the
/// given criterion, or `None` if neither smell-1 nor smell-2 fires. The
/// runner emits the returned finding (Warn severity, operator-visible
/// trace) and then `done failed` with cause [`PREFLIGHT_SMELL_CAUSE`].
///
/// Order is deliberate and contractual: smell-1 (huge baseline + zero
/// expected) is checked before smell-2 (canonical `grep -v 'mod tests'`)
/// so a criterion that trips both surfaces the more specific
/// false-positive signal first. Smell-3 (`wc -l` without `#[cfg(test)]`)
/// is excluded — it stays advisory and the runner handles it separately
/// after the blocking-smell short-circuit.
pub fn first_blocking_preflight_smell(
    cmd: &str,
    baseline: &str,
    expected: &str,
) -> Option<ReviewFinding> {
    smell_huge_baseline_zero_expected(cmd, baseline, expected)
        .or_else(|| smell_grep_v_mod_tests(cmd))
}

fn preflight_warn_finding(message: String) -> ReviewFinding {
    ReviewFinding {
        file: None,
        line: None,
        severity: Severity::Warn,
        origin: FindingOrigin::Mechanical {
            tool: PREFLIGHT_TOOL.into(),
            rule: None,
        },
        category: PREFLIGHT_CATEGORY.into(),
        message,
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

fn is_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

// ---------- auditor ra-query helpers (brief #87 phase2) ----------
//
// These are the parser primitives behind the auditor-claude-runner's two
// finding-emitting stages: `ra_query_unwraps_stage` (every unwrap site
// becomes one Warn finding) and `ra_query_callers_pubsurface_stage` (each
// new pub item with zero callers becomes one Warn `orphan_pub` finding).
// Both stages are best-effort: a missing `ra-query` binary, a parse error,
// or a non-zero exit shorts out the stage with a `*_skipped` event rather
// than failing the auditor.

/// Tool name embedded in `FindingOrigin::Mechanical` for findings emitted
/// by the ra-query-driven stages.
pub const RA_QUERY_TOOL: &str = "ra-query";

/// Category for the per-unwrap Warn finding stream (auditor stage 1 of 2
/// added in brief #87 phase2). The daemon never gates on Warns; this is
/// purely informational.
pub const UNWRAPS_CATEGORY: &str = "unwraps";

/// Category for the per-orphan-pub Warn finding stream (auditor stage 2
/// of 2 added in brief #87 phase2). A pub item is "orphan" iff it was
/// added in this brief (vs base_branch) AND has zero workspace callers.
pub const ORPHAN_PUB_CATEGORY: &str = "orphan_pub";

/// Parse one `ra-query unwraps <file> --format json` response into a list
/// of `Severity::Warn` findings, one per unwrap site. The `file` arg
/// names the source file ra-query was invoked on; the JSON's top-level
/// `file` field is preferred when present (and falls back to `file`).
///
/// Each finding's message is `<file>:<line>:<fqn>` per the brief, with
/// the unwrap method and severity appended in parens for triage. The
/// origin is `Mechanical { tool: "ra-query", rule: Some("unwraps") }`.
///
/// Returns an empty vec on any of: missing `functions` array, missing
/// `unwraps` array per function, or unparseable line numbers — the
/// caller treats absence of findings as "this file is clean".
pub fn parse_unwraps_findings(file: &str, json: &Value) -> Vec<ReviewFinding> {
    let mut out = Vec::new();
    let functions = match json.get("functions").and_then(Value::as_array) {
        Some(a) => a,
        None => return out,
    };
    let json_file = json
        .get("file")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| file.to_string());
    for fn_v in functions {
        let fn_name = fn_v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let unwraps = match fn_v.get("unwraps").and_then(Value::as_array) {
            Some(a) => a,
            None => continue,
        };
        for u in unwraps {
            let line_u64 = u.get("line").and_then(Value::as_u64).unwrap_or(0);
            let line: u32 = line_u64.try_into().unwrap_or(u32::MAX);
            let method = u.get("method").and_then(Value::as_str).unwrap_or("unwrap");
            let severity_str = u.get("severity").and_then(Value::as_str).unwrap_or("low");
            let message =
                format!("{json_file}:{line}:{fn_name} ({method}, severity={severity_str})");
            out.push(ReviewFinding {
                file: Some(json_file.clone()),
                line: Some(line),
                severity: Severity::Warn,
                origin: FindingOrigin::Mechanical {
                    tool: RA_QUERY_TOOL.into(),
                    rule: Some(UNWRAPS_CATEGORY.into()),
                },
                category: UNWRAPS_CATEGORY.into(),
                message,
                suggested_fix: None,
                prohibitions: Vec::new(),
                requirements: Vec::new(),
            });
        }
    }
    out
}

/// Parse `ra-query callers <pos> --format json` to a single caller count.
/// Mirrors the existing pub-surface stage's `.callers | length` jq
/// expression — a missing or non-array `callers` field reads as zero so
/// "shape we did not recognise" cannot turn into a Blocker downstream.
pub fn parse_callers_count(json: &Value) -> u64 {
    json.get("callers")
        .and_then(Value::as_array)
        .map(|a| a.len() as u64)
        .unwrap_or(0)
}

/// Build a single Warn finding for an orphan pub item (the
/// `ra_query_callers_pubsurface_stage` output unit). `file:line:fqn` is
/// the canonical message format shared with the unwraps stage.
pub fn orphan_pub_finding(file: &str, line: u32, fqn: &str, kind: &str) -> ReviewFinding {
    ReviewFinding {
        file: Some(file.to_string()),
        line: Some(line),
        severity: Severity::Warn,
        origin: FindingOrigin::Mechanical {
            tool: RA_QUERY_TOOL.into(),
            rule: Some(ORPHAN_PUB_CATEGORY.into()),
        },
        category: ORPHAN_PUB_CATEGORY.into(),
        message: format!("{file}:{line}:{fqn} (new pub {kind} with zero callers)"),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

/// Parse a `git diff --unified=0 <range>` text into a map from new-file
/// path to the set of new-file line numbers added by the diff. Used by
/// the orphan-pub stage to filter pub-surface output down to items
/// actually introduced by this brief.
///
/// Hunk headers carry the new-file start line as `+c[,d]`; with `-U0`
/// there are no context lines, so each `+`-prefixed line is at the
/// running counter and `-`-prefixed lines do not advance it.
pub fn parse_diff_added_lines(
    diff: &str,
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<u32>> {
    let mut map: std::collections::BTreeMap<String, std::collections::BTreeSet<u32>> =
        std::collections::BTreeMap::new();
    let mut cur_file: Option<String> = None;
    let mut cur_line: u32 = 0;
    for raw in diff.lines() {
        if let Some(rest) = raw.strip_prefix("+++ b/") {
            cur_file = Some(rest.to_string());
            continue;
        }
        if raw.starts_with("+++ /dev/null") {
            cur_file = None;
            continue;
        }
        if raw.starts_with("@@") {
            // "@@ -a[,b] +c[,d] @@ ..." — extract c.
            let plus_idx = match raw.find('+') {
                Some(i) => i,
                None => continue,
            };
            let after = &raw[plus_idx + 1..];
            let token: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .collect();
            let start: u32 = token
                .split(',')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            cur_line = start;
            continue;
        }
        if cur_file.is_none() {
            continue;
        }
        // Skip diff metadata lines that happen to start with '+' or '-'.
        if raw.starts_with("+++") || raw.starts_with("---") {
            continue;
        }
        if let Some(file) = &cur_file {
            if let Some(byte) = raw.as_bytes().first() {
                match *byte {
                    b'+' => {
                        map.entry(file.clone()).or_default().insert(cur_line);
                        cur_line = cur_line.saturating_add(1);
                    }
                    b'-' => {
                        // Removal — does not advance new-file counter.
                    }
                    _ => {
                        // With --unified=0 there are no context lines, but be
                        // permissive: treat anything else as a context line.
                        cur_line = cur_line.saturating_add(1);
                    }
                }
            }
        }
    }
    map
}

/// Build the `*_skipped` event payload that both ra-query stages emit
/// when they short-circuit (missing binary, git failure, parse error).
/// Stage label is verbatim in `msg` so dashboards can filter on it.
pub fn ra_query_skipped_event(stage: &str, reason: &str) -> Value {
    json!({
        "msg": format!("ra-query {stage} skipped"),
        "reason": reason,
    })
}
