//! preflight-criterion-runner — full role lifecycle for
//! preflight-criterion-agentry.
//!
//! EPIC #161 wave-bash port. Ports `PREFLIGHT_CRITERION_AGENTRY_SCRIPT`
//! (the issue-#84 baseline analyser) to a Rust runner binary. The role
//! is read-only on the workspace, has no forge auth, performs no git
//! push, and makes no HTTP calls — it just runs the brief's
//! `success_criteria` shell command against the workspace tip and
//! reports the baseline value plus heuristic smell-tests for
//! obviously-broken criteria. Does NOT gate; the planner consumes the
//! signal in brief 84b.
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! 1. Read startup bundle from stdin.
//! 2. Extract `success_criteria` and `target_repo` from `brief.payload`.
//! 3. Empty / missing `success_criteria` → emit error event + `done failed`.
//! 4. Split criterion on the FIRST occurrence of `" : "`
//!    (space-colon-space). cmd = before; expected = after, trimmed.
//!    Missing separator → emit error event + `done failed`.
//! 5. `cd /workspace`. On failure → emit error event + `done failed`.
//! 6. Run `bash -c "$cmd"` with stdout + stderr captured to separate
//!    buffers. baseline = trimmed stdout; stderr_tail = last 4096 bytes
//!    of stderr.
//! 7. Compare baseline vs expected; emit `baseline_match` event with
//!    baseline / expected / match / exit_code / stderr_tail.
//! 8. Apply heuristic smell-tests:
//!    - Smell 1: `expected == "0"`, baseline numeric > 100, cmd
//!      contains `wc -l` → Warn finding (likely false positives).
//!    - Smell 2: cmd contains `grep -v 'mod tests'` → Warn finding
//!      (canonical broken filter from #51).
//!    - Smell 3: cmd contains `wc -l` AND not `#[cfg(test)]` → Warn
//!      finding (test-scope exclusion missing).
//! 9. emit_done shipped — smells are Warn-only signal, never block.
//!
//! ## Verdict-routing parity with bash heredoc
//!
//! - Missing/empty success_criteria → Failed (matches bash).
//! - Missing space-colon-space separator → Failed (matches bash).
//! - cd /workspace failure → Failed (matches bash).
//! - Otherwise → Shipped, regardless of criterion exit code or smells
//!   (matches bash — non-zero exit is reported in the event payload but
//!   does not gate; smells are Warn findings, not Blockers).
//!
//! No intentional deviation from the bash variant routing.
//!
//! `DoneGuard` covers any unwound path so the daemon always sees a
//! terminal `done` event (EPIC #161 B0 invariant).

use std::process::{Command, Stdio};

use agentry_role_runtime::{
    emit_done, emit_event, emit_finding, pointer_str, read_bundle_value, smell_grep_v_mod_tests,
    smell_huge_baseline_zero_expected, smell_wc_l_without_cfg_test, split_criterion, tail_bytes,
    DoneGuard, CRITERION_OUTPUT_TAIL,
};
use orchestrator_types::EventVerdict;
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
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    let criterion = pointer_str(&bundle, "/brief/payload/success_criteria").to_string();
    let target_repo = pointer_str(&bundle, "/brief/payload/target_repo").to_string();

    if criterion.is_empty() {
        emit_event(json!({
            "error": "preflight-criterion missing success_criteria in payload",
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    let (cmd, expected) = match split_criterion(&criterion) {
        Some(parts) => parts,
        None => {
            emit_event(json!({
                "error": "success_criteria missing space-colon-space separator",
                "criterion": criterion,
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    if std::env::set_current_dir(WORKSPACE_DIR).is_err() {
        emit_event(json!({"error": "cd /workspace failed"}));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    emit_event(json!({
        "msg": "running preflight criterion",
        "cmd": cmd,
        "expected": expected,
        "target_repo": target_repo,
    }));

    let (exit_code, stdout_buf, stderr_buf) = run_criterion(&cmd);
    let baseline = trim_ascii_whitespace(&String::from_utf8_lossy(&stdout_buf));
    let stderr_tail = tail_bytes(&stderr_buf, CRITERION_OUTPUT_TAIL);
    let is_match = baseline == expected;

    emit_event(json!({
        "msg": "baseline_match",
        "baseline": baseline,
        "expected": expected,
        "match": is_match,
        "exit_code": exit_code,
        "stderr_tail": stderr_tail,
    }));

    if let Some(f) = smell_huge_baseline_zero_expected(&cmd, &baseline, &expected) {
        emit_finding(&f);
    }
    if let Some(f) = smell_grep_v_mod_tests(&cmd) {
        emit_finding(&f);
    }
    if let Some(f) = smell_wc_l_without_cfg_test(&cmd) {
        emit_finding(&f);
    }

    emit_done(EventVerdict::Shipped, None);
}

/// Run `bash -c "$cmd"` in `WORKSPACE_DIR`, capturing stdout and stderr
/// into separate buffers. Mirrors the bash heredoc's
/// `bash -c "$cmd" >stdout_file 2>stderr_file || exit_code=$?` shape.
///
/// On spawn failure (missing `bash` — should never happen in any sane
/// container), reports exit code `127` (POSIX command-not-found) with
/// the spawn-error message captured to the stderr buffer. On
/// signal-terminated child (no exit code), reports `1` — distinct from
/// success.
fn run_criterion(cmd: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let result = Command::new("bash")
        .args(["-c", cmd])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match result {
        Ok(o) => {
            let code = o.status.code().unwrap_or(1);
            (code, o.stdout, o.stderr)
        }
        Err(e) => (127, Vec::new(), format!("spawn bash: {e}").into_bytes()),
    }
}

/// Trim leading and trailing ASCII whitespace — POSIX `[[:space:]]*`
/// equivalent. Used to normalise the criterion's stdout into the
/// `baseline` value before the equality check against `expected`.
fn trim_ascii_whitespace(s: &str) -> String {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
        .to_string()
}
