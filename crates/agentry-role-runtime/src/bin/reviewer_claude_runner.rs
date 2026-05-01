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
use std::path::Path;
use std::process::{Command, Stdio};

use agentry_role_runtime::{
    changed_rs_files, emit_done, emit_event, emit_finding, head_bytes, parse_allowed_tools,
    parse_findings, pointer_str, ra_query_present, read_bundle_value, run_ra_query, stream_claude,
    tail_bytes, tail_lines, workspace_is_git_repo, DoneGuard, StreamErr,
};
use orchestrator_types::{DoneReason, EventVerdict, Severity};
use serde_json::{json, Value};

const WORKSPACE_DIR: &str = "/workspace";
const PANEL_TAIL_BUDGET: usize = 8192;
const PANEL_SUMMARY_BUDGET: usize = 512;
const ISSUE_BODY_BUDGET: usize = 2000;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
