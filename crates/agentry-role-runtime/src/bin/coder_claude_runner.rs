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
    body_has_verb_syntax, build_coder_prompt, build_self_review_prompt, emit_done, emit_event,
    emit_finding, is_v1_plus_topology, mech_finding, mech_finding_warn, parse_brief_context,
    parse_self_review_object, read_bundle_value, stream_claude, tail_bytes, tail_lines,
    BriefContext, DoneGuard, StreamErr,
};
use orchestrator_types::{DoneReason, EventVerdict, FindingOrigin, ReviewFinding, Severity};
use serde_json::{json, Value};

const WORKSPACE_DIR: &str = "/workspace";
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

    if !ctx.allowed_tools.is_empty() {
        emit_event(json!({
            "msg": "allowed_tools propagated from permit",
            "patterns": ctx.allowed_tools,
        }));
    }

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
