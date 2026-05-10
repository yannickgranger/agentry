//! reviewer-mechanical-runner — full role lifecycle for reviewer-mechanical-agentry.
//!
//! EPIC #161 Wave 2 final slice. Ports `REVIEWER_MECHANICAL_AGENTRY_SCRIPT`
//! (the mechanical reviewer bash heredoc) to a Rust runner binary. Reads the
//! coder's workspace read-only, re-runs the brief's `acceptance` command in
//! an isolated build dir (`CARGO_TARGET_DIR=/tmp/review-target`), emits
//! `shipped` on success and `rework_needed` on any non-zero exit (with the
//! tail of stderr/stdout bundled into a single Blocker finding).
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! 1. Read startup bundle on stdin.
//! 2. Extract `base_branch` (default `develop`) and `acceptance` (default
//!    `cargo test --workspace`) from `brief.payload`.
//! 3. Verify `/workspace/.git` exists (file or directory — `git worktree`
//!    surfaces `.git` as a regular file). Missing → emit error event +
//!    `done failed`.
//! 4. `cd /workspace`, emit `reviewer starting`.
//! 5. Run `git diff --stat <base_branch>...HEAD`, capture last line of
//!    stdout, emit `{msg:"diff",summary:<last>}`. Failure here is non-fatal.
//! 6. `export CARGO_TARGET_DIR=/tmp/review-target`, mkdir -p that path.
//! 7. Emit `running acceptance (isolated)` with the cmd.
//! 8. Run `sh -c "$acceptance"`, redirecting stdout to /tmp/rev.out and
//!    stderr to /tmp/rev.err (matches bash's `>/tmp/rev.out 2>/tmp/rev.err`).
//! 9. Exit 0 → `acceptance passed` event + `done shipped`.
//!    Exit non-zero → tail -50 stderr, tail -20 stdout, emit error event,
//!    build `combined = "{err_tail}\n---stdout---\n{out_tail}"` truncated to
//!    the first 2000 BYTES, emit one Blocker finding (cargo / acceptance),
//!    `done rework_needed`.
//!
//! `DoneGuard` covers any unwound path (panic, abrupt return) so the daemon
//! always sees a terminal `done` event (EPIC #161 B0 invariant).

use std::path::Path;
use std::process::{Command, Stdio};

use agentry_role_runtime::{
    build_reviewer_combined, emit_done, emit_event, emit_finding, mech_finding, pointer_str_or,
    read_bundle_value, tail_lines, DoneGuard,
};
use orchestrator_types::{DoneReason, EventVerdict};
use serde_json::json;

const WORKSPACE_DIR: &str = "/workspace";
const REVIEW_TARGET_DIR: &str = "/tmp/review-target";
const REV_STDOUT: &str = "/tmp/rev.out";
const REV_STDERR: &str = "/tmp/rev.err";
const STDERR_TAIL_LINES: usize = 50;
const STDOUT_TAIL_LINES: usize = 20;
const COMBINED_TRUNCATE_BYTES: usize = 2000;
const DEFAULT_BASE_BRANCH: &str = "develop";
const DEFAULT_ACCEPTANCE: &str = "cargo test --workspace";

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
                Some(DoneReason {
                    cause: "bundle_parse_failed".into(),
                    exit_code: None,
                    disagreements: Vec::new(),
                }),
            );
            return;
        }
    };

    let base_branch = pointer_str_or(&bundle, "/brief/payload/base_branch", DEFAULT_BASE_BRANCH);
    let acceptance = pointer_str_or(&bundle, "/brief/payload/acceptance", DEFAULT_ACCEPTANCE);

    let git_path = Path::new("/workspace/.git");
    if !git_path.exists() {
        emit_event(json!({
            "error": "workspace missing — coder did not produce it",
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    if std::env::set_current_dir(WORKSPACE_DIR).is_err() {
        emit_event(json!({"error": "cd /workspace failed"}));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    emit_event(json!({"msg": "reviewer starting"}));

    let diff_summary = run_diff_summary(&base_branch);
    emit_event(json!({"msg": "diff", "summary": diff_summary}));

    if std::fs::create_dir_all(REVIEW_TARGET_DIR).is_err() {
        emit_event(json!({
            "error": "mkdir -p /tmp/review-target failed",
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }
    std::env::set_var("CARGO_TARGET_DIR", REVIEW_TARGET_DIR);

    emit_event(json!({
        "msg": "running acceptance (isolated)",
        "cmd": acceptance,
    }));

    let exit_code = run_acceptance(&acceptance);

    if exit_code == 0 {
        emit_event(json!({"msg": "acceptance passed"}));
        emit_done(EventVerdict::Shipped, None);
        return;
    }

    let err_tail = read_tail_lines(REV_STDERR, STDERR_TAIL_LINES);
    let out_tail = read_tail_lines(REV_STDOUT, STDOUT_TAIL_LINES);

    emit_event(json!({
        "error": "acceptance failed",
        "stderr": err_tail,
        "stdout": out_tail,
    }));

    let combined = build_reviewer_combined(&err_tail, &out_tail, COMBINED_TRUNCATE_BYTES);
    emit_finding(&mech_finding("cargo", "acceptance", &combined));
    emit_done(EventVerdict::ReworkNeeded, None);
}

/// Run `git diff --stat <base_branch>...HEAD` and return the last non-empty
/// line of stdout. Empty string on any failure (matches bash `2>&1 | tail -1
/// || true`).
fn run_diff_summary(base_branch: &str) -> String {
    let range = format!("{base_branch}...HEAD");
    let result = Command::new("git")
        .args(["diff", "--stat", &range])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let output = match result {
        Ok(o) => o,
        Err(_) => return String::new(),
    };
    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    let s = String::from_utf8_lossy(&combined);
    s.lines().rfind(|l| !l.is_empty()).unwrap_or("").to_string()
}

/// Run `sh -c "$acceptance"` with stdout redirected to `/tmp/rev.out` and
/// stderr redirected to `/tmp/rev.err` (matches bash redirection exactly).
/// Returns the child's exit code; spawn failure → 127, signal-killed → 1.
fn run_acceptance(acceptance: &str) -> i32 {
    let stdout_file = match std::fs::File::create(REV_STDOUT) {
        Ok(f) => f,
        Err(e) => {
            emit_event(json!({
                "error": "failed to open /tmp/rev.out",
                "detail": e.to_string(),
            }));
            return 1;
        }
    };
    let stderr_file = match std::fs::File::create(REV_STDERR) {
        Ok(f) => f,
        Err(e) => {
            emit_event(json!({
                "error": "failed to open /tmp/rev.err",
                "detail": e.to_string(),
            }));
            return 1;
        }
    };

    let result = Command::new("sh")
        .args(["-c", acceptance])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .status();
    match result {
        Ok(s) => s.code().unwrap_or(1),
        Err(_) => 127,
    }
}

fn read_tail_lines(path: &str, n: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    tail_lines(&bytes, n)
}
