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
