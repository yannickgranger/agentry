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
//! - run `agentry_role_runtime::run_fence` against the diff: deterministic
//!   `ra-query` fence (clones / complexity / unwraps + callers fence) emits
//!   findings as Mechanical-origin `ReviewFinding`s. Findings are emitted to
//!   the trace BEFORE Claude runs so they cannot be downgraded by the LLM
//!   verdict. Y.5 fail-closed: substrate failure → single Blocker.
//! - build the strict review prompt (diff-only — structural concerns are
//!   covered deterministically by the fence; Claude does semantic review)
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
//! - verdict: fence-Blocker OR claude-Blocker → `done rework_needed`. The
//!   deterministic fence overrides Claude — Claude cannot downgrade a
//!   fence Blocker to a Shipped verdict. Otherwise → `done shipped`.
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
//! - The bash `ra-query` pre-pass was tolerant of a missing binary (skipped
//!   silently). The Y.6 fence is fail-closed: missing or broken `ra-query`
//!   is a substrate problem and emits one Blocker. Tolerance moved upstream
//!   to container warm-up (Y.7).
//!
//! - `DoneGuard` (EPIC #161 B0) covers any unwound path (panic, abrupt
//!   return) so the daemon always sees a terminal `done` event.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use agentry_role_runtime::{
    build_review_prompt_with_mechanical_findings, drop_empty_blocker_findings, emit_done,
    emit_event, emit_finding, format_mechanical_findings_summary, parse_allowed_tools,
    parse_findings, pointer_str, ra_query_pre_pass, read_bundle_value, run_fence,
    stream_claude_via_stdin, tail_lines, workspace_is_git_repo, DoneGuard, StreamErr,
};
use orchestrator_types::{DoneReason, EventVerdict, Severity};
use serde_json::json;

const WORKSPACE_DIR: &str = "/workspace";

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
    let allowed_tools = parse_allowed_tools(&bundle);
    if !allowed_tools.is_empty() {
        emit_event(json!({
            "msg": "allowed_tools propagated from permit",
            "patterns": allowed_tools,
        }));
    }

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

    // Deterministic fence runs FIRST, before Claude. Findings are emitted to
    // the trace immediately so they cannot be downgraded by Claude's verdict.
    let fence_findings = run_fence(Path::new(WORKSPACE_DIR), &base_branch);
    let fence_has_blocker = fence_findings
        .iter()
        .any(|f| matches!(f.severity, Severity::Blocker));
    for f in &fence_findings {
        emit_finding(f);
    }
    emit_event(json!({
        "msg": "fence pass complete",
        "findings_count": fence_findings.len(),
        "has_blocker": fence_has_blocker,
    }));

    // Phase 2.2 #87: ra-query informational pre-pass. Mechanical findings
    // (unwraps, complexity, dead pub items) caught structurally so the LLM
    // can focus on architectural review and avoid re-flagging them by
    // file:line. Skip-friendly: substrate failure does NOT block.
    let pre_pass = ra_query_pre_pass(Path::new(WORKSPACE_DIR), &base_branch);
    if let Some(reason) = pre_pass.skipped_reason.as_deref() {
        emit_event(json!({
            "msg": "ra-query pre-pass skipped",
            "reason": reason,
        }));
    } else {
        emit_event(json!({
            "msg": "ra-query pre-pass complete",
            "findings_count": pre_pass.findings.len(),
        }));
    }
    for f in &pre_pass.findings {
        emit_finding(f);
    }
    let mech_summary = format_mechanical_findings_summary(&pre_pass.findings);

    let prompt = build_review_prompt_with_mechanical_findings(
        &base_branch,
        &issue_title,
        &issue_body,
        &diff_text,
        &mech_summary,
    );

    let response = match stream_claude_via_stdin(&brief_id, ".reviewer", &prompt) {
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

    let mut claude_findings = parse_findings(&response, &agent_id);
    // #311 fence: drop Blocker findings whose message+requirements+prohibitions
    // are all empty. An empty Blocker is a parse failure, not a real defect;
    // letting it through would route the slice to ReworkNeeded with no
    // actionable signal for the coder respawn.
    let dropped_empty = drop_empty_blocker_findings(&mut claude_findings);
    if dropped_empty > 0 {
        emit_event(json!({
            "level": "warn",
            "msg": "dropped malformed Blocker findings (empty message+requirements+prohibitions)",
            "reviewer_agent_id": agent_id,
            "dropped": dropped_empty,
        }));
    }
    let claude_has_blocker = claude_findings
        .iter()
        .any(|f| matches!(f.severity, Severity::Blocker));
    emit_event(json!({
        "msg": "claude review parsed",
        "findings_count": claude_findings.len(),
        "dropped_empty_blockers": dropped_empty,
    }));
    for f in &claude_findings {
        emit_finding(f);
    }

    // Verdict: fence-Blocker OR claude-Blocker → ReworkNeeded.
    // Fence findings are deterministic and cannot be downgraded by Claude.
    let has_blocker = fence_has_blocker || claude_has_blocker;
    if has_blocker {
        emit_event(json!({
            "msg": "blockers present — requesting rework",
            "fence_blockers": fence_findings.iter().filter(|f| matches!(f.severity, Severity::Blocker)).count(),
            "claude_blockers": claude_findings.iter().filter(|f| matches!(f.severity, Severity::Blocker)).count(),
        }));
        emit_done(EventVerdict::ReworkNeeded, None);
    } else {
        emit_event(json!({"msg": "no blockers — reviewer passes"}));
        emit_done(EventVerdict::Shipped, None);
    }
}

fn fail_reason(cause: &str, exit_code: Option<i32>) -> Option<DoneReason> {
    Some(DoneReason {
        cause: cause.into(),
        exit_code,
        disagreements: Vec::new(),
    })
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
