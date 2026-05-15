//! verifier-dol-runner — full role lifecycle for verifier-claude-agentry.
//!
//! EPIC #161 Wave 3 final slice. Ports `VERIFIER_CLAUDE_AGENTRY_SCRIPT`
//! (the DOL verifier bash heredoc) to a Rust runner binary. Despite the
//! `claude` in the role name — kept for symmetry with the other agentry-*
//! roles — this verifier never invokes claude; it just runs the brief
//! payload's `success_criteria` shell command on a read-only snapshot of
//! the workspace and emits `done shipped` / `done failed` based on the
//! command's exit code.
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! 1. Read startup bundle on stdin.
//! 2. Extract `success_criteria` (required) and `verifies_brief_id`
//!    (optional) from `brief.payload`.
//! 3. Empty / missing `success_criteria` → emit error event + done failed.
//! 4. `cd /workspace`, emit `running success_criteria` event with the
//!    criterion + verifies pointer for trace context.
//! 5. Execute the criterion via `sh -c "$criterion"`, capturing combined
//!    stdout+stderr.
//! 6. Map exit code → verdict:
//!    - exit 0: emit `criterion passed` + done shipped
//!    - exit non-zero: emit `criterion failed` (with exit_code + tail of
//!      combined output) + done failed
//!
//! Mirrors bash `tail -c 4096` — the last 4096 BYTES of combined output
//! are attached to the event payload.
//!
//! `DoneGuard` covers any unwound path (panic, abrupt return) so the
//! daemon always sees a terminal `done` event (EPIC #161 B0 invariant).

use std::process::{Command, Stdio};

use agentry_role_runtime::{
    emit_done, emit_event, parse_success_criteria, parse_verifies_brief_id, read_bundle_value,
    tail_bytes, verdict_for_exit_code, DoneGuard, CRITERION_OUTPUT_TAIL,
};
use orchestrator_types::{DoneReason, EventVerdict};
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
                Some(DoneReason {
                    cause: "bundle_parse_failed".into(),
                    exit_code: None,
                    disagreements: Vec::new(),
                }),
            );
            return;
        }
    };

    let criterion = match parse_success_criteria(&bundle) {
        Some(c) => c,
        None => {
            emit_event(json!({
                "error": "verifier missing success_criteria in payload",
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };
    let verifies = parse_verifies_brief_id(&bundle);

    if std::env::set_current_dir(WORKSPACE_DIR).is_err() {
        emit_event(json!({"error": "cd /workspace failed"}));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    emit_event(json!({
        "msg": "running success_criteria",
        "criterion": criterion,
        "verifies": verifies,
    }));

    let (exit_code, output) = run_criterion(&criterion);
    let tail = tail_bytes(&output, CRITERION_OUTPUT_TAIL);

    if matches!(verdict_for_exit_code(exit_code), EventVerdict::Shipped) {
        emit_event(json!({
            "msg": "criterion passed",
            "output": tail,
        }));
        emit_done(EventVerdict::Shipped, None);
    } else {
        emit_event(json!({
            "msg": "criterion failed",
            "exit_code": exit_code,
            "output": tail,
        }));
        emit_done(
            EventVerdict::Failed,
            Some(DoneReason {
                cause: "criterion_failed".into(),
                exit_code: Some(exit_code),
                disagreements: Vec::new(),
            }),
        );
    }
}

/// Execute `sh -c "$criterion"` in `WORKSPACE_DIR`, capturing combined
/// stdout+stderr. Returns `(exit_code, combined_output)`.
///
/// On spawn failure (missing `sh` — should never happen in any sane
/// container), reports exit code `127` (POSIX command-not-found) with the
/// spawn-error message as the output. On signal-terminated child (no
/// exit code), reports `1` — distinct from success and consistent with
/// bash's `128+signal` capture.
fn run_criterion(criterion: &str) -> (i32, Vec<u8>) {
    let result = Command::new("sh")
        .args(["-c", criterion])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let out = match result {
        Ok(o) => o,
        Err(e) => return (127, format!("spawn sh: {e}").into_bytes()),
    };
    let mut combined = out.stdout;
    combined.extend_from_slice(&out.stderr);
    let code = out.status.code().unwrap_or(1);
    (code, combined)
}
