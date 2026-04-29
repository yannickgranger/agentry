//! reviewer-claude-runner — code-review role: diff → claude → findings.
//!
//! Ports `REVIEWER_CLAUDE_AGENTRY_SCRIPT` (~220 LoC bash) under EPIC #161
//! Wave 1.4. Same shape as ac-verifier-runner (workspace prep, host binary
//! invocation, finding emission), with the added stream-claude pattern that
//! coder-claude (Wave 1.2) will reuse.
//!
//! Behaviour preserved verbatim from bash:
//!
//! - read startup bundle on stdin
//! - extract `brief.id`, `permit.agent_id`,
//!   `brief.payload.{base_branch,issue_title,issue_body}`
//! - workspace not a git repo → `done failed` (no salvage; coder is upstream)
//! - `git diff <base_branch>...HEAD` (3-dot, symmetric range — what HEAD
//!   added vs the branchpoint) — failure or empty diff → `done failed`
//! - emit `reviewing diff` event with `diff_bytes`
//! - optional ra-query pre-pass: walk `*.rs` files in the diff, run
//!   `ra-query unwraps --severity high` and `ra-query complexity --threshold 15`,
//!   emit a panel event, and prepend a short summary to the prompt
//! - build the strict review prompt (verbatim copy from bash)
//! - stream `claude -p --output-format stream-json --verbose <prompt>` to
//!   `/transcripts/<brief_id>.reviewer.jsonl`, mirroring each line as a
//!   trace event (`{claude:<obj>}` for parseable lines, `{claude_raw:<str>}`
//!   for malformed ones — same wire shape as bash `stream_claude`)
//! - on claude failure → emit error event + `done failed`
//! - reconstruct assistant final text from transcript: prefer `type=result`
//!   line's `.result` field; fall back to concatenated `assistant.message
//!   .content[].text`
//! - strip optional ` ``` ` / ` ```json ` fences, slice between the first
//!   `[` and last `]`, parse as JSON array of findings
//! - on slice/parse failure → salvage as a single `format_deviation`
//!   finding wrapping the head of the prose-only reply (severity Blocker;
//!   bash had `severity:"error"` here which the daemon couldn't deserialize
//!   anyway — see PORT NOTES below)
//! - emit one `Finding` per parsed finding (model origin, agent_id from
//!   permit, prohibitions/requirements arrays preserved)
//! - any blocker → `done rework_needed`; otherwise → `done shipped`
//!
//! ## PORT NOTES
//!
//! - The bash salvage path emitted `severity: "error"` which is not in the
//!   `Severity` enum (`Blocker | Warn`). Daemon-side deserialization would
//!   reject it. The Rust port uses `Severity::Blocker` for salvaged
//!   findings — the format deviation IS a deal-breaker (no usable review
//!   was produced, the rework loop must fire) so blocker is the closest
//!   correct mapping. This is the only intentional semantic departure.
//!
//! - Bash kept the `ra-query` pre-pass tolerant of missing binary; Rust
//!   port mirrors via `Command::spawn` returning `ErrorKind::NotFound`.
//!
//! - `DoneGuard` (EPIC #161 B0) covers any unwound path (panic, abrupt
//!   return) so the daemon always sees a terminal `done` event.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use agentry_role_runtime::{emit_done, emit_event, emit_finding, DoneGuard};
use orchestrator_types::{DoneReason, EventVerdict, FindingOrigin, ReviewFinding, Severity};
use serde_json::{json, Value};

const TRANSCRIPTS_DIR: &str = "/transcripts";
const WORKSPACE_DIR: &str = "/workspace";
const PANEL_TAIL_BUDGET: usize = 8192;
const PANEL_SUMMARY_BUDGET: usize = 512;
const ISSUE_BODY_BUDGET: usize = 2000;
const SALVAGE_BUDGET: usize = 4096;

fn main() {
    let _guard = DoneGuard::new();

    let bundle = match read_bundle_value() {
        Ok(v) => v,
        Err(e) => {
            emit_event(json!({
                "error": "failed to parse startup bundle",
                "detail": e.to_string(),
            }));
            emit_done(
                EventVerdict::Failed,
                fail_reason("bundle_parse_failed", None),
            );
            return;
        }
    };

    let brief_id = pointer_str(&bundle, "/brief/id").to_string();
    let base_branch = {
        let s = pointer_str(&bundle, "/brief/payload/base_branch");
        if s.is_empty() {
            "develop".to_string()
        } else {
            s.to_string()
        }
    };
    let issue_title = pointer_str(&bundle, "/brief/payload/issue_title").to_string();
    let issue_body = pointer_str(&bundle, "/brief/payload/issue_body").to_string();
    let agent_id = pointer_str(&bundle, "/permit/agent_id").to_string();

    if !workspace_is_git_repo(WORKSPACE_DIR) {
        emit_event(json!({
            "error": "workspace is not a git repo — coder did not produce it",
        }));
        emit_done(EventVerdict::Failed, fail_reason("workspace_not_git", None));
        return;
    }

    // The bash sets HOME=/root before invoking claude; Rust mirrors via the
    // child env block (so the runner's own HOME is untouched).
    let _ = fs::create_dir_all("/root/.claude");

    let diff_text = match git_diff_3dot(&base_branch) {
        Ok(d) => d,
        Err(err) => {
            emit_event(json!({
                "error": "git diff failed",
                "detail": err,
            }));
            emit_done(EventVerdict::Failed, fail_reason("git_diff_failed", None));
            return;
        }
    };

    if diff_text.is_empty() {
        emit_event(json!({"error": "empty diff — coder produced no changes"}));
        emit_done(EventVerdict::Failed, fail_reason("empty_diff", None));
        return;
    }

    emit_event(json!({
        "msg": "reviewing diff",
        "diff_bytes": diff_text.len(),
    }));

    let panel_summary = run_ra_query_pre_pass(&diff_text);
    let prompt = build_review_prompt(
        &base_branch,
        &issue_title,
        &issue_body,
        &diff_text,
        &panel_summary,
    );

    let response = match stream_claude(&brief_id, ".reviewer", &prompt) {
        Ok(r) => r,
        Err(StreamErr::ClaudeFailed { exit_code, detail }) => {
            emit_event(json!({
                "error": "claude -p failed",
                "exit_code": exit_code,
                "detail": detail,
            }));
            emit_done(
                EventVerdict::Failed,
                fail_reason("claude_failed", Some(exit_code)),
            );
            return;
        }
        Err(StreamErr::TranscriptEmpty { path }) => {
            emit_event(json!({
                "error": "tee_or_transcript_write_failed",
                "transcript_path": path,
            }));
            emit_done(EventVerdict::Failed, fail_reason("transcript_empty", None));
            return;
        }
    };

    let findings = parse_findings(&response, &agent_id);

    let count = findings.len();
    emit_event(json!({
        "msg": "claude review parsed",
        "findings_count": count,
    }));

    let mut has_blocker = false;
    for f in &findings {
        if matches!(f.severity, Severity::Blocker) {
            has_blocker = true;
        }
        emit_finding(f);
    }

    if has_blocker {
        emit_event(json!({"msg": "blockers present — requesting rework"}));
        emit_done(EventVerdict::ReworkNeeded, None);
    } else {
        emit_event(json!({"msg": "no blockers — claude-reviewer passes"}));
        emit_done(EventVerdict::Shipped, None);
    }
}

fn fail_reason(cause: &str, exit_code: Option<i32>) -> Option<DoneReason> {
    Some(DoneReason {
        cause: cause.into(),
        exit_code,
    })
}

fn read_bundle_value() -> anyhow::Result<Value> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| anyhow::anyhow!("read stdin: {e}"))?;
    serde_json::from_str(&buf).map_err(|e| anyhow::anyhow!("parse bundle: {e}"))
}

fn pointer_str<'a>(bundle: &'a Value, ptr: &str) -> &'a str {
    bundle.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

fn workspace_is_git_repo(workspace: &str) -> bool {
    // Mirrors `[ -d /workspace/.git ] || [ -f /workspace/.git ]`. Worktrees
    // present as files; full clones present as directories.
    let dot_git = Path::new(workspace).join(".git");
    dot_git.is_dir() || dot_git.is_file()
}

fn git_diff_3dot(base_branch: &str) -> Result<String, String> {
    // 3-dot range: what HEAD added relative to the merge-base with base_branch.
    // This is what bash uses (`git diff "${base_branch}...HEAD"`) — the symmetric
    // form that scopes the review to commits unique to HEAD.
    let range = format!("{base_branch}...HEAD");
    let out = Command::new("git")
        .arg("diff")
        .arg(&range)
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git diff: {e}"))?;
    if !out.status.success() {
        let mut combined = Vec::with_capacity(out.stdout.len() + out.stderr.len());
        combined.extend_from_slice(&out.stdout);
        combined.extend_from_slice(&out.stderr);
        return Err(tail_lines(&combined, 20));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Walk the `*.rs` files touched by the diff, run `ra-query unwraps` and
/// `ra-query complexity` against each, emit a panel event, return a short
/// human-readable summary string for prompt embedding.
///
/// Tolerates missing `ra-query` binary: emits one `ra_query_unavailable`
/// event and returns empty string. Mirrors the bash `command -v ra-query`
/// guard — operators who haven't run `just ra-query-binary` still get a
/// usable review, just without the pre-pass anchors.
fn run_ra_query_pre_pass(diff_text: &str) -> String {
    if !ra_query_present() {
        emit_event(json!({
            "msg": "ra_query_unavailable",
            "detail": "skipping reviewer pre-pass",
        }));
        return String::new();
    }

    let changed = changed_rs_files(diff_text);
    let mut panel = Vec::with_capacity(changed.len());
    for f in &changed {
        let abs = Path::new(WORKSPACE_DIR).join(f);
        if !abs.is_file() {
            continue;
        }
        let abs_s = abs.to_string_lossy().into_owned();
        let unwraps = run_ra_query(&["unwraps", &abs_s, "--severity", "high", "--format", "json"])
            .unwrap_or_else(|_| json!({"functions": []}));
        let complexity = run_ra_query(&[
            "complexity",
            &abs_s,
            "--threshold",
            "15",
            "--format",
            "json",
        ])
        .unwrap_or_else(|_| json!({"functions": []}));
        panel.push(json!({
            "file": f,
            "unwraps": unwraps,
            "complexity": complexity,
        }));
    }

    let panel_value = Value::Array(panel.clone());
    let panel_text = serde_json::to_string(&panel_value).unwrap_or_else(|_| "[]".into());
    let panel_tail = tail_bytes(panel_text.as_bytes(), PANEL_TAIL_BUDGET);
    emit_event(json!({
        "msg": "ra_query_review_panel",
        "findings_json_tail": panel_tail,
    }));

    let mut lines: Vec<String> = Vec::new();
    for entry in &panel {
        let unwraps_total = entry
            .pointer("/unwraps/summary/total")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let complexity_total = entry
            .pointer("/complexity/summary/total")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if unwraps_total + complexity_total == 0 {
            continue;
        }
        let file = entry.get("file").and_then(Value::as_str).unwrap_or("");
        lines.push(format!(
            "{file}: unwraps={unwraps_total} complexity_hot={complexity_total}"
        ));
    }
    let summary = lines.join("\n");
    if summary.len() > PANEL_SUMMARY_BUDGET {
        // Match `head -c 512` truncation. Cut on a UTF-8 char boundary to
        // avoid panics on multi-byte boundary slices.
        let mut cut = PANEL_SUMMARY_BUDGET;
        while !summary.is_char_boundary(cut) {
            cut -= 1;
        }
        summary[..cut].to_string()
    } else {
        summary
    }
}

fn ra_query_present() -> bool {
    Command::new("ra-query")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn run_ra_query(args: &[&str]) -> Result<Value, String> {
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
fn changed_rs_files(diff_text: &str) -> Vec<String> {
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

fn build_review_prompt(
    base_branch: &str,
    issue_title: &str,
    issue_body: &str,
    diff_text: &str,
    panel_summary: &str,
) -> String {
    // Verbatim from REVIEWER_CLAUDE_AGENTRY_SCRIPT — including the strict
    // output-format guidance, scope guardrail, verb-completeness check, and
    // the four CRITICAL audits (role-spec, bootstrap-command,
    // daemon-lifecycle, state-machine idempotency). Any prose drift here
    // changes reviewer behaviour mid-port.
    let issue_body_head = head_bytes(issue_body, ISSUE_BODY_BUDGET);
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
    if !panel_summary.is_empty() {
        s.push_str(&format!(
            "\n--- Mechanical findings from ra-query (unwraps>=high, complexity>=15) ---\n\
             {panel_summary}\n\
             --- End mechanical findings ---\n"
        ));
    }
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

enum StreamErr {
    ClaudeFailed { exit_code: i32, detail: String },
    TranscriptEmpty { path: String },
}

/// Spawn `timeout $CLAUDE_P_TIMEOUT claude -p --output-format stream-json
/// --verbose <prompt>`, mirror each stdout line as a structured event AND
/// append it to `/transcripts/<brief_id><suffix>.jsonl`. After the child
/// exits, parse the transcript for the assistant's final text.
///
/// Mirrors the bash `stream_claude` helper bit-for-bit: same env (HOME=/root),
/// same timeout-via-coreutils binary, same wire shape on stdout, same
/// transcript layout.
fn stream_claude(brief_id: &str, suffix: &str, prompt: &str) -> Result<String, StreamErr> {
    let _ = fs::create_dir_all(TRANSCRIPTS_DIR);
    let transcript_path = format!("{TRANSCRIPTS_DIR}/{brief_id}{suffix}.jsonl");

    let mut transcript = match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&transcript_path)
    {
        Ok(f) => f,
        Err(e) => {
            return Err(StreamErr::ClaudeFailed {
                exit_code: -1,
                detail: format!("open transcript {transcript_path}: {e}"),
            });
        }
    };

    let timeout_secs = std::env::var("CLAUDE_P_TIMEOUT").unwrap_or_else(|_| "1200".into());

    // bash: `HOME=/root timeout "$CLAUDE_P_TIMEOUT" claude -p --output-format stream-json --verbose "$_prompt" 2>&1`
    let mut cmd = Command::new("timeout");
    cmd.arg(&timeout_secs)
        .arg("claude")
        .arg("-p")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg(prompt)
        .env("HOME", "/root")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Bash redirected 2>&1 so stderr ends up in the same stream the
        // tee/while-read pipeline consumes. Mirror by merging stderr into
        // stdout via a pipe + interleaved drain.
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(StreamErr::ClaudeFailed {
                exit_code: -1,
                detail: format!("spawn timeout(1) claude -p: {e}"),
            });
        }
    };

    // Read stdout line-by-line. Stderr is drained in a sibling thread to
    // avoid back-pressure on the child if claude logs there.
    let stdout = child
        .stdout
        .take()
        .expect("piped stdout not connected to child");
    let stderr = child.stderr.take();

    let stderr_handle = stderr.map(|s| {
        std::thread::spawn(move || {
            let mut tail = Vec::new();
            let mut buf = [0u8; 4096];
            let mut s = s;
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => tail.extend_from_slice(&buf[..n]),
                }
            }
            // Keep only the last 4 KiB so a chatty claude can't blow up
            // memory; mirrors bash's eventual `tail -c` semantics on err.
            if tail.len() > 4096 {
                let cut = tail.len() - 4096;
                tail.drain(..cut);
            }
            String::from_utf8_lossy(&tail).into_owned()
        })
    });

    let reader = BufReader::new(stdout);
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => break,
        };
        if writeln!(transcript, "{line}").is_err() {
            // Transcript write failure is detected post-wait via the
            // empty-file guard below — mirroring bash's defence-in-depth.
        }
        emit_claude_line(&line);
    }
    let _ = transcript.flush();

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            return Err(StreamErr::ClaudeFailed {
                exit_code: -1,
                detail: format!("wait child: {e}"),
            });
        }
    };
    let stderr_tail = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let exit_code = status.code().unwrap_or(-1);
    if !status.success() {
        return Err(StreamErr::ClaudeFailed {
            exit_code,
            detail: stderr_tail,
        });
    }

    // Bash defence-in-depth: claude reported success but tee couldn't write
    // the transcript (rootless podman subuid mismatch on /transcripts).
    let meta = fs::metadata(&transcript_path).map_err(|e| StreamErr::ClaudeFailed {
        exit_code,
        detail: format!("stat transcript {transcript_path}: {e}"),
    })?;
    if meta.len() == 0 {
        return Err(StreamErr::TranscriptEmpty {
            path: transcript_path,
        });
    }

    Ok(reconstruct_assistant_text(&transcript_path))
}

fn emit_claude_line(line: &str) {
    if let Ok(parsed) = serde_json::from_str::<Value>(line) {
        emit_event(json!({"claude": parsed}));
    } else {
        emit_event(json!({"claude_raw": line}));
    }
}

/// Walk the transcript, prefer the `result` event's `.result` field; if
/// missing, concatenate `assistant.message.content[].text` segments. Bash
/// behaviour was identical (and the latter fallback was added to avoid
/// `tail -1` truncating multi-line JSON).
fn reconstruct_assistant_text(transcript_path: &str) -> String {
    let f = match fs::File::open(transcript_path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let reader = BufReader::new(f);
    let mut result_field: Option<String> = None;
    let mut assistant_chunks: Vec<String> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(Value::as_str) {
            Some("result") => {
                if let Some(r) = v.get("result").and_then(Value::as_str) {
                    result_field = Some(r.to_string());
                }
            }
            Some("assistant") => {
                let content = v.pointer("/message/content").and_then(Value::as_array);
                if let Some(arr) = content {
                    for c in arr {
                        if c.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(t) = c.get("text").and_then(Value::as_str) {
                                assistant_chunks.push(t.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    result_field.unwrap_or_else(|| assistant_chunks.join(""))
}

/// Strip optional code fences, slice between first `[` and last `]`, parse
/// as a JSON array of finding-shape objects. On any failure, salvage the
/// reply as a single `format_deviation` Blocker finding so the rework loop
/// has a concrete handle. Returns the emit-ready findings.
pub(crate) fn parse_findings(response: &str, agent_id: &str) -> Vec<ReviewFinding> {
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

fn strip_fences(raw: &str) -> String {
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

fn slice_json_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end < start {
        return None;
    }
    Some(&text[start..=end])
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

fn parse_severity(s: Option<&str>) -> Severity {
    match s.unwrap_or("warn") {
        "blocker" => Severity::Blocker,
        _ => Severity::Warn,
    }
}

fn string_array_field(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn head_bytes(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    let mut cut = budget;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

fn tail_lines(buf: &[u8], n: usize) -> String {
    let s = String::from_utf8_lossy(buf);
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn tail_bytes(buf: &[u8], n: usize) -> String {
    let start = buf.len().saturating_sub(n);
    String::from_utf8_lossy(&buf[start..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn build_review_prompt_includes_diff_and_title() {
        let prompt = build_review_prompt("develop", "Fix bug", "BODY", "DIFF_TEXT", "");
        assert!(prompt.contains("TITLE: Fix bug"));
        assert!(prompt.contains("DIFF_TEXT"));
        assert!(prompt.contains("Output EXACTLY a JSON array"));
        assert!(!prompt.contains("--- Mechanical findings"));
    }

    #[test]
    fn build_review_prompt_includes_panel_summary_when_present() {
        let prompt = build_review_prompt(
            "develop",
            "T",
            "B",
            "D",
            "src/x.rs: unwraps=2 complexity_hot=0",
        );
        assert!(prompt.contains("--- Mechanical findings"));
        assert!(prompt.contains("unwraps=2"));
    }

    #[test]
    fn reconstruct_assistant_text_prefers_result_field() {
        // Write a minimal transcript with both a result line and an
        // assistant line. The result field should win.
        let dir = std::env::temp_dir().join("agentry_role_runtime_recon_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.jsonl");
        let _ = std::fs::write(
            &path,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"draft\"}]}}\n\
             {\"type\":\"result\",\"result\":\"final\"}\n",
        );
        let s = reconstruct_assistant_text(path.to_str().expect("tempdir path is utf8"));
        assert_eq!(s, "final");
    }

    #[test]
    fn reconstruct_assistant_text_falls_back_to_assistant_chunks() {
        let dir = std::env::temp_dir().join("agentry_role_runtime_recon_test_2");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.jsonl");
        let _ = std::fs::write(
            &path,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"part1\"}]}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"part2\"}]}}\n",
        );
        let s = reconstruct_assistant_text(path.to_str().expect("tempdir path is utf8"));
        assert_eq!(s, "part1part2");
    }
}
