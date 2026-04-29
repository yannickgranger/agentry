//! coder-claude-runner — full role lifecycle for coder-claude-agentry.
//!
//! EPIC #161 Wave 1.2a ported the entrypoint half (bundle parsing, rework
//! banner, prompt build, claude streaming). Wave 1.2b ports the exitpoint
//! half (cargo fmt, quality-hygiene, acceptance eval, self-review claude
//! soft-fail, dead-pub-check, git commit) and merges both into this single
//! binary.
//!
//! With both halves owned by one Rust process the cross-language
//! `/tmp/brief_vars.sh` IPC handle disappears, role state lives in typed
//! Rust structs, and `DoneGuard` works the standard way (any unwound path
//! emits `done failed` so the daemon always sees a terminal event).
//!
//! ## Phases
//!
//! 1. **Entrypoint phase** — read bundle, parse fields, walk
//!    `team_context.messages[].payload.findings[]` for prior blockers,
//!    build the rework banner if applicable, set git config, write the
//!    verb-structured prompt, stream `claude -p` to a transcript.
//! 2. **v1+ topology shortcut** — if `topology_name` ends in `-vN` for
//!    `N >= 1`, skip the local commit/push pipeline (the orchestrator's
//!    `git-operator` role handles those for v1+ topologies). Run a
//!    best-effort `cargo fmt --all` and ship.
//! 3. **Exitpoint phase** (v0 only) — `cargo fmt --all` (hard),
//!    `quality-hygiene --fix` (optional), `eval "$acceptance"` (hard),
//!    `git add -A` + has-staged check, optional self-review claude call
//!    (soft-fail — degrades to `all_applied: true` on transient claude
//!    error), optional `dead-pub-check`, then `git commit`.
//!
//! Behaviour mirrors `BASH_PRELUDE + CODER_CLAUDE_AGENTRY_SCRIPT +
//! CODER_CLAUDE_AGENTRY_EXITPOINT` bit-for-bit, including the soft-fail
//! semantics of self-review (so a flaky claude call cannot kill an
//! otherwise-correct coder run) and the optional-binary tolerance for
//! `quality-hygiene` and `dead-pub-check`.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use agentry_role_runtime::{
    emit_done, emit_event, emit_finding, head_bytes, mech_finding, pointer_str, pointer_str_or,
    read_bundle_value, stream_claude, string_array_field, strip_fences, tail_bytes, tail_lines,
    DoneGuard, StreamErr,
};
use orchestrator_types::{DoneReason, EventVerdict, FindingOrigin, ReviewFinding, Severity};
use serde_json::{json, Value};

const WORKSPACE_DIR: &str = "/workspace";
const ISSUE_BODY_BUDGET: usize = 3000;
const FMT_ERR_TAIL: usize = 50;
const HYGIENE_ERR_TAIL: usize = 100;
const ACCEPTANCE_ERR_TAIL: usize = 50;
const DEAD_PUB_ERR_TAIL: usize = 4096;

fn main() {
    let _guard = DoneGuard::new();
    if let Err(err) = run() {
        emit_event(json!({
            "error": err.event_msg,
            "detail": err.detail,
        }));
        emit_done(
            EventVerdict::Failed,
            Some(DoneReason {
                cause: err.cause.into(),
                exit_code: err.exit_code,
            }),
        );
    }
    // On Ok the run() body has already called emit_done(Shipped|Failed)
    // explicitly via the shipping path. DoneGuard catches anything else.
}

#[derive(Debug)]
struct RunErr {
    event_msg: &'static str,
    detail: String,
    cause: &'static str,
    exit_code: Option<i32>,
}

fn run() -> Result<(), RunErr> {
    let bundle = read_bundle_value().map_err(|e| RunErr {
        event_msg: "failed to parse startup bundle",
        detail: e.to_string(),
        cause: "bundle_parse_failed",
        exit_code: None,
    })?;

    let ctx = parse_brief_context(&bundle);

    if std::env::var("GITEA_TOKEN")
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        return Err(RunErr {
            event_msg: "GITEA_TOKEN not in env",
            detail: String::new(),
            cause: "gitea_token_missing",
            exit_code: None,
        });
    }

    let _ = fs::create_dir_all("/root/.claude");

    git_config_global(&[
        ("user.email", "coder-claude-agentry@agentry.lab"),
        ("user.name", "coder-claude-agentry"),
        ("http.sslVerify", "false"),
    ])
    .map_err(|e| RunErr {
        event_msg: "git config --global failed",
        detail: e,
        cause: "git_config_failed",
        exit_code: None,
    })?;

    // ----- Entrypoint phase: rework banner, prompt, claude stream -----

    if !ctx.blocker_findings.is_empty() {
        emit_event(json!({
            "msg": "rework iteration — injecting prior findings into prompt",
            "blocker_count": ctx.blocker_findings.len(),
        }));
    }

    emit_event(json!({
        "msg": "workspace worktree ready",
        "branch": ctx.branch,
    }));

    let prompt = build_coder_prompt(
        &ctx.base_branch,
        &ctx.branch,
        &ctx.rework_banner,
        &ctx.issue_title,
        &ctx.issue_body,
        &ctx.acceptance,
    );
    emit_event(json!({
        "msg": "calling claude -p",
        "prompt_bytes": prompt.len(),
    }));

    let reply = match stream_claude(&ctx.brief_id, ".coder", &prompt) {
        Ok(r) => r,
        Err(StreamErr::ClaudeFailed { exit_code, detail }) => {
            return Err(RunErr {
                event_msg: "claude -p failed",
                detail,
                cause: "claude_failed",
                exit_code: Some(exit_code),
            });
        }
        Err(StreamErr::TranscriptEmpty { path }) => {
            return Err(RunErr {
                event_msg: "tee_or_transcript_write_failed",
                detail: path,
                cause: "transcript_empty",
                exit_code: None,
            });
        }
    };
    emit_event(json!({
        "msg": "claude reply received",
        "bytes": reply.len(),
    }));

    // ----- v1+ topology shortcut -----

    if is_v1_plus_topology(&ctx.topology_name) {
        emit_event(json!({
            "msg": "v1+ topology — skipping coder-side commit/push (git-operator role does it)",
        }));
        // Best-effort fmt; ignore failure (parity with bash `2>/dev/null || true`).
        run_cargo_fmt_quiet();
        emit_done(EventVerdict::Shipped, None);
        return Ok(());
    }

    // ----- Exitpoint phase (v0 topologies) -----

    exitpoint_phase(&ctx)
}

// ---------------------------------------------------------------------------
// Brief context
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct BriefContext {
    brief_id: String,
    base_branch: String,
    issue_title: String,
    issue_body: String,
    acceptance: String,
    branch: String,
    topology_name: String,
    rework_banner: String,
    blocker_findings: Vec<PriorFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PriorFinding {
    message: String,
    prohibitions: Vec<String>,
    requirements: Vec<String>,
}

fn parse_brief_context(bundle: &Value) -> BriefContext {
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
    }
}

fn collect_blocker_findings(bundle: &Value) -> Vec<PriorFinding> {
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

fn build_rework_banner(findings: &[PriorFinding]) -> String {
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

fn build_coder_prompt(
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
         \n\
         When the transformations are complete and the acceptance passes, simply\n\
         report success and exit.\n"
    )
}

// ---------------------------------------------------------------------------
// Topology check
// ---------------------------------------------------------------------------

/// Bash regex `-v[1-9][0-9]*$` — true for `agentry-self-host-v1`,
/// `agentry-self-host-v12`, etc. False for v0 (the v0 topology runs the
/// local commit/push exitpoint path).
fn is_v1_plus_topology(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 3 {
        return false;
    }
    // Walk back from the tail collecting digits.
    let mut digit_start = bytes.len();
    while digit_start > 0 && bytes[digit_start - 1].is_ascii_digit() {
        digit_start -= 1;
    }
    if digit_start == bytes.len() {
        // No trailing digits.
        return false;
    }
    // Char before the digit run must be 'v', and char before that must be '-'.
    if digit_start < 2 || bytes[digit_start - 1] != b'v' || bytes[digit_start - 2] != b'-' {
        return false;
    }
    let digits = &bytes[digit_start..];
    // First digit must be 1..=9 (no leading zero, no v0).
    let first = digits[0];
    (b'1'..=b'9').contains(&first) && digits[1..].iter().all(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Exitpoint phase
// ---------------------------------------------------------------------------

fn exitpoint_phase(ctx: &BriefContext) -> Result<(), RunErr> {
    // 1. Baseline cargo fmt (hard fail)
    emit_event(json!({"msg": "running cargo fmt --all (baseline)"}));
    if let Err(err) = run_cargo_fmt() {
        let detail = tail_lines(&err, FMT_ERR_TAIL);
        emit_event(json!({
            "error": "cargo fmt --all failed",
            "detail": detail,
        }));
        emit_finding(&mech_finding("cargo-fmt", "fmt", &detail));
        emit_done(
            EventVerdict::Failed,
            Some(DoneReason {
                cause: "cargo_fmt_failed".into(),
                exit_code: None,
            }),
        );
        return Ok(());
    }
    emit_event(json!({"msg": "cargo fmt --all clean"}));

    // 2. quality-hygiene --fix (optional)
    if which_on_path("quality-hygiene") {
        emit_event(json!({"msg": "running quality-hygiene --fix"}));
        if let Err(err) = run_quality_hygiene(&ctx.base_branch) {
            let detail = tail_lines(&err, HYGIENE_ERR_TAIL);
            emit_event(json!({
                "error": "quality-hygiene --fix failed",
                "detail": detail,
            }));
            emit_finding(&mech_finding("quality-hygiene", "hygiene", &detail));
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause: "quality_hygiene_failed".into(),
                    exit_code: None,
                }),
            );
            return Ok(());
        }
        emit_event(json!({"msg": "quality-hygiene --fix clean"}));
    } else {
        emit_event(json!({"msg": "quality-hygiene not installed, skipping role-local gate"}));
    }

    // 3. Acceptance self-check (hard fail) — `eval "$acceptance"` ≡ `sh -c "$acceptance"`
    if let Err(err) = run_acceptance(&ctx.acceptance) {
        let detail = tail_lines(&err, ACCEPTANCE_ERR_TAIL);
        emit_event(json!({
            "error": "acceptance failed (self-check)",
            "detail": detail,
        }));
        emit_finding(&mech_finding("cargo", "acceptance", &detail));
        emit_done(
            EventVerdict::Failed,
            Some(DoneReason {
                cause: "acceptance_failed".into(),
                exit_code: None,
            }),
        );
        return Ok(());
    }
    emit_event(json!({"msg": "acceptance passed (self-check)"}));

    // 4. git add -A + has-staged check
    git_add_all().map_err(|e| RunErr {
        event_msg: "git add -A failed",
        detail: e,
        cause: "git_add_failed",
        exit_code: None,
    })?;
    if !git_has_staged_changes().map_err(|e| RunErr {
        event_msg: "git diff --cached check failed",
        detail: e,
        cause: "git_check_failed",
        exit_code: None,
    })? {
        emit_event(json!({"error": "no changes produced"}));
        emit_done(
            EventVerdict::Failed,
            Some(DoneReason {
                cause: "no_changes".into(),
                exit_code: None,
            }),
        );
        return Ok(());
    }

    // 5. Self-review (verb completeness) — soft-fail
    if body_has_verb_syntax(&ctx.issue_body) {
        let staged_diff = git_diff_cached().unwrap_or_default();
        if run_self_review(ctx, &staged_diff).is_err() {
            // run_self_review already emitted its own done failed.
            return Ok(());
        }
    }

    // 6. dead-pub-check (optional)
    if which_on_path("dead-pub-check") {
        run_dead_pub_check_phase();
    } else {
        emit_event(json!({
            "msg": "dead_pub_check_unavailable",
            "detail": "binary not on PATH; coder gate skipped",
        }));
    }

    // 7. git commit
    let sha = git_commit(&format!("auto({}): {}", ctx.brief_id, ctx.issue_title)).map_err(|e| {
        RunErr {
            event_msg: "git commit failed",
            detail: e,
            cause: "git_commit_failed",
            exit_code: None,
        }
    })?;
    emit_event(json!({
        "msg": "committed",
        "branch": ctx.branch,
        "sha": sha,
    }));

    emit_done(EventVerdict::Shipped, None);
    Ok(())
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

fn git_config_global(pairs: &[(&str, &str)]) -> Result<(), String> {
    for (k, v) in pairs {
        let out = Command::new("git")
            .arg("config")
            .arg("--global")
            .arg(k)
            .arg(v)
            .current_dir(WORKSPACE_DIR)
            .output()
            .map_err(|e| format!("spawn git config {k}: {e}"))?;
        if !out.status.success() {
            let detail = String::from_utf8_lossy(&out.stderr);
            return Err(format!("git config {k}={v}: {detail}"));
        }
    }
    let dot_git = Path::new(WORKSPACE_DIR).join(".git");
    if !dot_git.is_dir() && !dot_git.is_file() {
        return Err(format!("{WORKSPACE_DIR} is not a git repo"));
    }
    Ok(())
}

fn which_on_path(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn run_cargo_fmt() -> Result<(), Vec<u8>> {
    let out = Command::new("cargo")
        .arg("fmt")
        .arg("--all")
        .current_dir(WORKSPACE_DIR)
        .output()
        .map_err(|e| format!("spawn cargo fmt: {e}").into_bytes())?;
    if out.status.success() {
        Ok(())
    } else {
        let mut combined = out.stderr;
        if !out.stdout.is_empty() {
            combined.extend_from_slice(b"\n---stdout---\n");
            combined.extend_from_slice(&out.stdout);
        }
        Err(combined)
    }
}

fn run_cargo_fmt_quiet() {
    let _ = Command::new("cargo")
        .arg("fmt")
        .arg("--all")
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn run_quality_hygiene(base_branch: &str) -> Result<(), Vec<u8>> {
    let out = Command::new("quality-hygiene")
        .arg("--fix")
        .arg("--workspace")
        .arg(WORKSPACE_DIR)
        .arg("--base")
        .arg(base_branch)
        .current_dir(WORKSPACE_DIR)
        .output()
        .map_err(|e| format!("spawn quality-hygiene: {e}").into_bytes())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(out.stderr)
    }
}

fn run_acceptance(acceptance: &str) -> Result<(), Vec<u8>> {
    // Bash uses `eval "$acceptance"`; sh -c is the POSIX equivalent.
    // Brief author is trusted (claude itself dispatches), but isolate the
    // working dir to /workspace and don't propagate the runner's stdin.
    let out = Command::new("sh")
        .arg("-c")
        .arg(acceptance)
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn sh -c acceptance: {e}").into_bytes())?;
    if out.status.success() {
        Ok(())
    } else {
        let mut combined = out.stderr;
        if !out.stdout.is_empty() {
            combined.extend_from_slice(b"\n---stdout---\n");
            combined.extend_from_slice(&out.stdout);
        }
        Err(combined)
    }
}

fn git_add_all() -> Result<(), String> {
    let out = Command::new("git")
        .arg("add")
        .arg("-A")
        .current_dir(WORKSPACE_DIR)
        .output()
        .map_err(|e| format!("spawn git add: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(())
}

fn git_has_staged_changes() -> Result<bool, String> {
    let out = Command::new("git")
        .arg("diff")
        .arg("--cached")
        .arg("--quiet")
        .current_dir(WORKSPACE_DIR)
        .output()
        .map_err(|e| format!("spawn git diff --cached --quiet: {e}"))?;
    // `--quiet` returns 0 when no diff, 1 when there is a diff.
    Ok(!out.status.success())
}

fn git_diff_cached() -> Result<String, String> {
    let out = Command::new("git")
        .arg("diff")
        .arg("--cached")
        .current_dir(WORKSPACE_DIR)
        .output()
        .map_err(|e| format!("spawn git diff --cached: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_diff_cached_u0() -> Result<String, String> {
    let out = Command::new("git")
        .arg("diff")
        .arg("--cached")
        .arg("-U0")
        .current_dir(WORKSPACE_DIR)
        .output()
        .map_err(|e| format!("spawn git diff --cached -U0: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_commit(message: &str) -> Result<String, String> {
    let out = Command::new("git")
        .arg("commit")
        .arg("-m")
        .arg(message)
        .current_dir(WORKSPACE_DIR)
        .output()
        .map_err(|e| format!("spawn git commit: {e}"))?;
    if !out.status.success() {
        let mut combined = out.stderr;
        if !out.stdout.is_empty() {
            combined.extend_from_slice(b"\n---stdout---\n");
            combined.extend_from_slice(&out.stdout);
        }
        return Err(String::from_utf8_lossy(&combined).into_owned());
    }
    let sha = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(WORKSPACE_DIR)
        .output()
        .map_err(|e| format!("spawn git rev-parse HEAD: {e}"))?;
    if !sha.status.success() {
        return Err(String::from_utf8_lossy(&sha.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&sha.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Self-review (soft-fail claude call)
// ---------------------------------------------------------------------------

fn body_has_verb_syntax(body: &str) -> bool {
    // Bash: `grep -qE '^(### [0-9]+\. |CREATE |UPDATE |REPLACE |DELETE |MOVE )'`
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
    // `### N. ` heading style (e.g. `### 1. CREATE foo.rs`, `### 12. UPDATE ...`).
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

fn build_self_review_prompt(issue_body: &str, staged_diff: &str) -> String {
    let body_head = head_bytes(issue_body, ISSUE_BODY_BUDGET);
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
         in the diff at the named location. Output EXACTLY a JSON object — no\n\
         markdown fences, no prose:\n\
         \n\
         {{\n\
         \x20\x20\"all_applied\": true,\n\
         \x20\x20\"unapplied\": []\n\
         }}\n\
         \n\
         If any verb is missing, set all_applied:false and list each missing verb\n\
         as a short description (max 200 chars each, max 6 entries).\n\
         \n\
         Your response, right now, starting with {{ and ending with }}:\n",
    )
}

#[derive(Debug, PartialEq, Eq)]
struct SelfReviewResult {
    all_applied: bool,
    unapplied: Vec<String>,
}

fn parse_self_review_object(raw: &str) -> Option<SelfReviewResult> {
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
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Some(SelfReviewResult {
        all_applied,
        unapplied,
    })
}

fn slice_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&text[start..=end])
}

/// Run the self-review claude call. Returns `Ok(())` to continue the
/// pipeline (verbs all applied OR malformed reply tolerated). Returns
/// `Err(())` when the call surfaced unapplied verbs and the role-local
/// `done failed` was already emitted — caller should return without
/// continuing.
fn run_self_review(ctx: &BriefContext, staged_diff: &str) -> Result<(), ()> {
    emit_event(json!({"msg": "running self-review (verb completeness)"}));
    let prompt = build_self_review_prompt(&ctx.issue_body, staged_diff);
    let reply = match stream_claude(&ctx.brief_id, ".self-review", &prompt) {
        Ok(r) => r,
        Err(StreamErr::ClaudeFailed { exit_code, .. }) => {
            // Soft-fail: degrade to all_applied:true. Bash's
            // `set -e + pipefail` wrapper had identical semantics.
            emit_event(json!({
                "warn": "self-review claude call failed; proceeding",
                "exit_code": exit_code,
            }));
            return Ok(());
        }
        Err(StreamErr::TranscriptEmpty { path }) => {
            emit_event(json!({
                "warn": "self-review transcript empty; proceeding",
                "transcript_path": path,
            }));
            return Ok(());
        }
    };

    let parsed = match parse_self_review_object(&reply) {
        Some(p) => p,
        None => {
            // No JSON braces / not an object — fall through to commit.
            // Reviewer-claude is the architectural backstop.
            if reply.contains('{') && reply.contains('}') {
                emit_event(json!({
                    "warn": "self-review response not a JSON object; proceeding"
                }));
            } else {
                emit_event(json!({
                    "warn": "self-review response missing JSON braces; proceeding"
                }));
            }
            return Ok(());
        }
    };

    if parsed.all_applied {
        emit_event(json!({"msg": "self-review: all verbs applied"}));
        return Ok(());
    }

    for item in &parsed.unapplied {
        emit_finding(&ReviewFinding {
            file: None,
            line: None,
            severity: Severity::Blocker,
            origin: FindingOrigin::Model {
                reviewer_agent_id: "coder-self-review".into(),
            },
            category: "completeness".into(),
            message: format!("unapplied verb: {item}"),
            suggested_fix: None,
            prohibitions: Vec::new(),
            requirements: Vec::new(),
        });
    }
    emit_event(json!({"error": "self-review found unapplied verbs"}));
    emit_done(
        EventVerdict::Failed,
        Some(DoneReason {
            cause: "self_review_unapplied".into(),
            exit_code: None,
        }),
    );
    Err(())
}

// ---------------------------------------------------------------------------
// dead-pub-check JSONL pipeline
// ---------------------------------------------------------------------------

fn run_dead_pub_check_phase() {
    emit_event(json!({"msg": "running dead-pub-check"}));
    let diff_text = match git_diff_cached_u0() {
        Ok(d) => d,
        Err(e) => {
            emit_event(json!({
                "warn": "dead-pub-check: git diff --cached -U0 failed",
                "detail": e,
            }));
            return;
        }
    };
    let stdin_payload = json!({
        "diff": diff_text,
        "workspace_root": WORKSPACE_DIR,
    })
    .to_string();
    let mut cmd = Command::new("dead-pub-check");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            emit_event(json!({
                "warn": "dead-pub-check failed",
                "detail": format!("spawn: {e}"),
            }));
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_payload.as_bytes());
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            emit_event(json!({
                "warn": "dead-pub-check failed",
                "detail": format!("wait: {e}"),
            }));
            return;
        }
    };
    if !out.status.success() {
        let detail = tail_bytes(&out.stderr, DEAD_PUB_ERR_TAIL);
        emit_event(json!({
            "warn": "dead-pub-check failed",
            "detail": detail,
        }));
        return;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let sev = v.get("severity").and_then(Value::as_str).unwrap_or("warn");
        let category = v
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("dead-pub")
            .to_string();
        let message = v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("<malformed finding>")
            .to_string();
        if sev == "warn" {
            emit_finding(&mech_finding_warn("ra-query", &category, &message));
        } else {
            emit_event(json!({
                "msg": "dead_pub_info",
                "severity": sev,
                "category": category,
                "detail": message,
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Emit helpers + small string helpers
// ---------------------------------------------------------------------------

fn mech_finding_warn(tool: &str, category: &str, message: &str) -> ReviewFinding {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collect_blocker_findings_filters_by_severity() {
        let bundle = json!({
            "team_context": {
                "messages": [{"payload": {"findings": [
                    {"severity": "blocker", "message": "x", "prohibitions": ["a"], "requirements": ["b"]},
                    {"severity": "warn", "message": "y"}
                ]}}]
            }
        });
        let v = collect_blocker_findings(&bundle);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].message, "x");
        assert_eq!(v[0].prohibitions, vec!["a"]);
    }

    #[test]
    fn build_rework_banner_joins_with_semicolons_and_keeps_dollar_brace_literal() {
        let f = vec![PriorFinding {
            message: "bad".into(),
            prohibitions: vec!["p1".into(), "p2".into()],
            requirements: vec!["r1".into()],
        }];
        let b = build_rework_banner(&f);
        assert!(b.contains("- BLOCKER: bad"));
        assert!(b.contains("Prohibitions: p1; p2"));
        assert!(b.contains("Requirements: r1"));
        assert!(b.contains("git diff ${base_branch}...HEAD"));
    }

    #[test]
    fn build_coder_prompt_contains_required_anchors() {
        let p = build_coder_prompt(
            "develop",
            "auto/brf_x",
            "",
            "Fix bug",
            "CREATE foo.rs:10",
            "cargo test",
        );
        assert!(p.contains("branch \"develop\""));
        assert!(p.contains("branch \"auto/brf_x\""));
        assert!(p.contains("Task title: Fix bug"));
        assert!(p.contains("CREATE foo.rs:10"));
        assert!(p.contains("cargo test"));
    }

    #[test]
    fn is_v1_plus_topology_matches_v1_through_v99() {
        assert!(is_v1_plus_topology("agentry-self-host-v1"));
        assert!(is_v1_plus_topology("agentry-self-host-v2"));
        assert!(is_v1_plus_topology("agentry-self-host-v9"));
        assert!(is_v1_plus_topology("agentry-self-host-v12"));
        assert!(is_v1_plus_topology("foo-v123"));
    }

    #[test]
    fn is_v1_plus_topology_rejects_v0_and_other_shapes() {
        assert!(!is_v1_plus_topology("agentry-self-host-v0"));
        assert!(!is_v1_plus_topology("agentry-self-host"));
        assert!(!is_v1_plus_topology(""));
        assert!(!is_v1_plus_topology("v1")); // no leading '-'
        assert!(!is_v1_plus_topology("agentry-v")); // empty digits
        assert!(!is_v1_plus_topology("agentry-vx1")); // leading non-digit before v
        assert!(!is_v1_plus_topology("agentry-v01")); // leading zero
        assert!(!is_v1_plus_topology("agentry-self-host-v1-extra")); // suffix after digits
    }

    #[test]
    fn body_has_verb_syntax_recognises_bare_verbs() {
        assert!(body_has_verb_syntax("CREATE foo.rs:10\nUPDATE bar.rs:20"));
        assert!(body_has_verb_syntax("UPDATE foo.rs:10"));
        assert!(body_has_verb_syntax("REPLACE foo.rs:10"));
        assert!(body_has_verb_syntax("DELETE foo.rs:10"));
        assert!(body_has_verb_syntax("MOVE foo.rs:10 -> bar.rs:20"));
    }

    #[test]
    fn body_has_verb_syntax_recognises_numbered_headings() {
        assert!(body_has_verb_syntax("### 1. CREATE foo.rs"));
        assert!(body_has_verb_syntax("### 12. UPDATE foo.rs"));
        assert!(body_has_verb_syntax("preamble\n### 3. MOVE foo.rs"));
    }

    #[test]
    fn body_has_verb_syntax_rejects_free_form() {
        assert!(!body_has_verb_syntax("just a description"));
        assert!(!body_has_verb_syntax(""));
        assert!(!body_has_verb_syntax("create foo.rs"));
        assert!(!body_has_verb_syntax("CREATEx foo.rs"));
        assert!(!body_has_verb_syntax("### CREATE foo.rs"));
        assert!(!body_has_verb_syntax("# CREATE foo.rs"));
    }

    #[test]
    fn slice_json_object_picks_outer_braces() {
        assert_eq!(
            slice_json_object("prefix{\"a\":1}suffix"),
            Some("{\"a\":1}")
        );
        assert_eq!(
            slice_json_object("garbage {\"x\":\"y\"}"),
            Some("{\"x\":\"y\"}")
        );
    }

    #[test]
    fn slice_json_object_returns_none_when_missing() {
        assert_eq!(slice_json_object("no braces"), None);
        assert_eq!(slice_json_object("} only closing"), None);
        assert_eq!(slice_json_object("{ only opening"), None);
    }

    #[test]
    fn parse_self_review_object_extracts_fields() {
        let raw = r#"```json
        {"all_applied": false, "unapplied": ["verb 1", "verb 2"]}
        ```"#;
        let r = parse_self_review_object(raw).expect("parse");
        assert!(!r.all_applied);
        assert_eq!(r.unapplied, vec!["verb 1", "verb 2"]);
    }

    #[test]
    fn parse_self_review_object_defaults_all_applied_when_absent() {
        let raw = r#"{"unapplied": []}"#;
        let r = parse_self_review_object(raw).expect("parse");
        assert!(r.all_applied);
        assert!(r.unapplied.is_empty());
    }

    #[test]
    fn parse_self_review_object_returns_none_for_prose() {
        assert_eq!(parse_self_review_object("just prose"), None);
        assert_eq!(parse_self_review_object("} backwards {"), None);
    }

    #[test]
    fn parse_self_review_object_returns_none_for_array() {
        assert_eq!(parse_self_review_object("[1, 2, 3]"), None);
    }

    #[test]
    fn build_self_review_prompt_includes_diff_and_body_head() {
        let p = build_self_review_prompt("CREATE foo.rs:10", "DIFF_TEXT");
        assert!(p.contains("CREATE foo.rs:10"));
        assert!(p.contains("DIFF_TEXT"));
        assert!(p.contains("all_applied"));
        assert!(p.contains("unapplied"));
        assert!(p.contains("starting with { and ending with }"));
    }

    #[test]
    fn build_self_review_prompt_truncates_long_body() {
        // Use a marker char that doesn't appear elsewhere in the prompt
        // template — the longest run of consecutive markers is the body
        // head, capped at ISSUE_BODY_BUDGET.
        let body = "Q".repeat(5000);
        let p = build_self_review_prompt(&body, "diff");
        let longest_q_run = p.split(|c: char| c != 'Q').map(str::len).max().unwrap_or(0);
        assert_eq!(longest_q_run, ISSUE_BODY_BUDGET);
    }

    #[test]
    fn mech_finding_warn_uses_warn_severity() {
        let f = mech_finding_warn("ra-query", "dead-pub", "x is unused");
        assert!(matches!(f.severity, Severity::Warn));
        assert_eq!(f.category, "dead-pub");
    }

    #[test]
    fn parse_brief_context_pulls_all_fields() {
        let bundle = json!({
            "brief": {
                "id": "brf_42",
                "topology": {"name": "agentry-self-host-v0"},
                "payload": {
                    "issue_title": "T",
                    "issue_body": "BODY",
                    "acceptance": "cargo test",
                    "base_branch": "develop"
                }
            },
            "permit": {"agent_id": "agt_xyz"},
            "team_context": {"messages": []}
        });
        let ctx = parse_brief_context(&bundle);
        assert_eq!(ctx.brief_id, "brf_42");
        assert_eq!(ctx.branch, "auto/brf_42");
        assert_eq!(ctx.topology_name, "agentry-self-host-v0");
        assert_eq!(ctx.acceptance, "cargo test");
        assert_eq!(ctx.base_branch, "develop");
        assert_eq!(ctx.issue_title, "T");
        assert_eq!(ctx.issue_body, "BODY");
        assert!(ctx.rework_banner.is_empty());
        assert!(ctx.blocker_findings.is_empty());
    }

    #[test]
    fn parse_brief_context_builds_rework_banner_with_findings() {
        let bundle = json!({
            "brief": {
                "id": "brf_1",
                "topology": {"name": "agentry-self-host-v0"},
                "payload": {"issue_body": "x"}
            },
            "permit": {"agent_id": "a"},
            "team_context": {
                "messages": [{"payload": {"findings": [
                    {"severity": "blocker", "message": "rework me", "prohibitions": [], "requirements": []}
                ]}}]
            }
        });
        let ctx = parse_brief_context(&bundle);
        assert!(ctx.rework_banner.contains("REWORK iteration"));
        assert!(ctx.rework_banner.contains("rework me"));
        assert_eq!(ctx.blocker_findings.len(), 1);
    }
}
