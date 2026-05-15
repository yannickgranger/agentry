//! pr-rebaser-runner — full role lifecycle for pr-rebaser-agentry.
//!
//! EPIC #161 wave-bash port. Replaces `PR_REBASER_AGENTRY_SCRIPT` (~80
//! LoC bash heredoc) with a Rust runner binary, mirroring the
//! ci-watcher-runner / shipper-runner pattern. The role's
//! `entrypoint_script` becomes a one-line shell wrapper that execs
//! `/usr/local/bin/pr-rebaser-runner`.
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! 1. Read startup bundle on stdin, extract the chained
//!    `pr-rebaser_brief.json` payload (target_repo, pr_number, branch,
//!    base_branch, forge_host). `forge_host` arrives via the phase-3
//!    daemon cascade through `agentry.toml [forge] default_host` — the
//!    bash literal `// "forge.example.com:3000"` fallback is gone.
//! 2. Workspace must be a git repo — missing `.git` → `done failed`.
//! 3. `cd /workspace`, set git user.email/user.name (idempotent).
//! 4. `git fetch origin <base_branch>`; failure → `done failed`.
//! 5. `git fetch origin <branch>`; failure → `done failed`.
//! 6. `git checkout <branch>`; failure → `done failed`.
//! 7. `git rebase origin/<base_branch>`; classify via
//!    [`agentry_role_runtime::pr_rebaser::classify_rebase`]:
//!    - `Success`: capture new HEAD sha, `git push --force-with-lease
//!      origin <branch>` to the token-bearing remote, emit_message
//!      addressed to `ci-watcher-agentry` so the original watcher can
//!      retry the merge, `done shipped`.
//!    - `Conflict`: emit one Blocker finding per unmerged path,
//!      `git rebase --abort`, emit a structured event with the file
//!      list, `done rework_needed`. Mirrors the bash original's
//!      `emit_done "rework_needed"` so the daemon's review-producer
//!      routing rewinds the chain to upstream rework rather than
//!      terminating the brief permanently.
//!    - `Fatal`: emit the non-conflict diagnostic, `git rebase --abort`,
//!      `done failed`.
//!
//! `DoneGuard` covers any unwound path (panic, abrupt return) so the
//! daemon always sees a terminal `done` event (EPIC #161 B0 invariant).

use std::process::{Command, Stdio};

use agentry_role_runtime::pr_rebaser::{
    classify_rebase, compose_remote_url, parse_rebaser_payload, parse_unmerged_files,
    push_force_with_lease_args, PayloadError, RebaseOutcome, RebaserPayload,
};
use agentry_role_runtime::{
    emit_done, emit_event, emit_finding, emit_message, read_bundle_value, tail_lines,
    workspace_is_git_repo, DoneGuard,
};
use orchestrator_types::{DoneReason, EventVerdict, FindingOrigin, ReviewFinding, Severity};
use serde_json::json;

const WORKSPACE_DIR: &str = "/workspace";
const FETCH_ERR_TAIL_LINES: usize = 10;
const REBASE_DIAG_TAIL_LINES: usize = 30;

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

    let payload = match parse_rebaser_payload(&bundle) {
        Ok(p) => p,
        Err(PayloadError::MissingBranch) => {
            emit_event(json!({"error": "branch missing in brief.payload"}));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    if !workspace_is_git_repo(WORKSPACE_DIR) {
        emit_event(json!({
            "error": "workspace missing — no .git found at /workspace",
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    if std::env::set_current_dir(WORKSPACE_DIR).is_err() {
        emit_event(json!({"error": "cd /workspace failed"}));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    // Idempotent — `git config` overwrites the existing value rather than
    // appending. Mirrors the bash `git config user.email/user.name`.
    let _ = run_git(&["config", "user.email", "pr-rebaser@agentry.lab"]);
    let _ = run_git(&["config", "user.name", "pr-rebaser"]);

    emit_event(json!({
        "msg": "pr-rebaser starting",
        "branch": payload.branch,
        "base_branch": payload.base_branch,
        "pr_number": payload.pr_number,
        "target_repo": payload.target_repo,
        "forge_host": payload.forge_host,
    }));

    if let Err(detail) = git_fetch(&payload.base_branch) {
        emit_event(json!({
            "error": "git fetch base failed",
            "base": payload.base_branch,
            "detail": detail,
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    if let Err(detail) = git_fetch(&payload.branch) {
        emit_event(json!({
            "error": "git fetch branch failed",
            "branch": payload.branch,
            "detail": detail,
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    if let Err(detail) = git_checkout(&payload.branch) {
        emit_event(json!({
            "error": "git checkout failed",
            "branch": payload.branch,
            "detail": detail,
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    let base_sha = match capture_git(&["rev-parse", &format!("origin/{}", payload.base_branch)]) {
        Ok((0, out, _)) => String::from_utf8_lossy(&out).trim().to_string(),
        _ => String::new(),
    };

    let (rc, rebase_out, rebase_err) =
        match capture_git(&["rebase", &format!("origin/{}", payload.base_branch)]) {
            Ok(t) => t,
            Err(e) => {
                emit_event(json!({
                    "error": "git rebase spawn failed",
                    "detail": e,
                }));
                emit_done(EventVerdict::Failed, None);
                return;
            }
        };

    let mut combined = rebase_out.clone();
    combined.extend_from_slice(&rebase_err);

    let status_out = match capture_git(&["status", "--porcelain=v2", "-uno"]) {
        Ok((_, out, _)) => String::from_utf8_lossy(&out).into_owned(),
        Err(_) => String::new(),
    };

    match classify_rebase(rc, &status_out) {
        RebaseOutcome::Success => {
            handle_rebase_success(&payload, &base_sha);
        }
        RebaseOutcome::Conflict => {
            handle_rebase_conflict(&payload, &status_out);
        }
        RebaseOutcome::Fatal => {
            let detail = tail_lines(&combined, REBASE_DIAG_TAIL_LINES);
            let _ = run_git(&["rebase", "--abort"]);
            emit_event(json!({
                "error": "git rebase failed (non-conflict)",
                "branch": payload.branch,
                "detail": detail,
            }));
            emit_done(EventVerdict::Failed, None);
        }
    }
}

fn handle_rebase_success(payload: &RebaserPayload, base_sha: &str) {
    let new_sha = match capture_git(&["rev-parse", "HEAD"]) {
        Ok((0, out, _)) => String::from_utf8_lossy(&out).trim().to_string(),
        _ => String::new(),
    };

    let token = match std::env::var("GITEA_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            emit_event(json!({
                "error": "GITEA_TOKEN not in env — cannot push rebased branch",
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    let remote_url = compose_remote_url(&payload.forge_host, &payload.target_repo, &token);
    let mut args = push_force_with_lease_args(&payload.branch);
    // Replace `origin` with the token-bearing URL so the push authenticates
    // against the forge without relying on a pre-configured credential
    // helper inside the container.
    if let Some(idx) = args.iter().position(|s| s == "origin") {
        args[idx] = remote_url;
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    match capture_git(&argv) {
        Ok((0, _, _)) => {}
        Ok((_, _, err)) => {
            let detail = tail_lines(&err, FETCH_ERR_TAIL_LINES);
            emit_event(json!({
                "error": "git push --force-with-lease failed",
                "branch": payload.branch,
                "detail": detail,
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
        Err(e) => {
            emit_event(json!({
                "error": "git push spawn failed",
                "branch": payload.branch,
                "detail": e,
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    }

    emit_event(json!({
        "msg": "rebased and pushed",
        "rebased": true,
        "branch": payload.branch,
        "base_sha": base_sha,
        "new_sha": new_sha,
    }));

    // Re-trigger ci-watcher with the new head sha so it re-polls CI on the
    // freshly rebased commit and retries the merge.
    emit_message(
        "ci-watcher-agentry",
        json!({
            "pr_number": payload.pr_number,
            "new_head_sha": new_sha,
        }),
    );

    emit_done(EventVerdict::Shipped, None);
}

fn handle_rebase_conflict(payload: &RebaserPayload, status_out: &str) {
    // Verdict is `ReworkNeeded` — verbatim port of the bash original's
    // `emit_done "rework_needed"`. The per-file Blocker findings emitted
    // above travel as separate `EventKind::Finding` events; the daemon
    // accumulates them and routes the rewind via the team's
    // ReworkNeeded edges. `Failed` would terminate the brief permanently
    // and bypass that routing.
    let unmerged = parse_unmerged_files(status_out);
    for f in &unmerged {
        emit_finding(&conflict_finding(f));
    }
    let _ = run_git(&["rebase", "--abort"]);
    emit_event(json!({
        "msg": "rebase conflicts — aborted, requesting rework",
        "branch": payload.branch,
        "files": unmerged,
    }));
    emit_done(EventVerdict::ReworkNeeded, None);
}

fn conflict_finding(file: &str) -> ReviewFinding {
    ReviewFinding {
        file: Some(file.to_string()),
        line: None,
        severity: Severity::Blocker,
        origin: FindingOrigin::Mechanical {
            tool: "pr-rebaser".into(),
            rule: Some("rebase-conflict".into()),
        },
        category: "rebase-conflict".into(),
        message: format!("rebase conflict in {file}"),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

fn git_fetch(refspec: &str) -> Result<(), String> {
    match capture_git(&["fetch", "origin", refspec]) {
        Ok((0, _, _)) => Ok(()),
        Ok((_, _, err)) => Err(tail_lines(&err, FETCH_ERR_TAIL_LINES)),
        Err(e) => Err(e),
    }
}

fn git_checkout(branch: &str) -> Result<(), String> {
    match capture_git(&["checkout", branch]) {
        Ok((0, _, _)) => Ok(()),
        Ok((_, _, err)) => Err(tail_lines(&err, FETCH_ERR_TAIL_LINES)),
        Err(e) => Err(e),
    }
}

/// Run `git <args>` in the current directory, returning
/// `(exit_code, stdout, stderr)`. Spawn errors bubble up via `Err(String)`.
fn capture_git(args: &[&str]) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
    let out = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    let code = out.status.code().unwrap_or(-1);
    Ok((code, out.stdout, out.stderr))
}

/// Run `git <args>` and discard output. Used for fire-and-forget calls
/// (`git config`, `git rebase --abort`) where the bash original used
/// `>/dev/null 2>&1 || true`.
fn run_git(args: &[&str]) -> Result<(), ()> {
    Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
        .map_err(|_| ())
}
