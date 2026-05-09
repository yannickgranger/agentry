//! shipper-runner — full role lifecycle for shipper-agentry.
//!
//! EPIC #161 wave-bash port of `SHIPPER_AGENTRY_SCRIPT` (~70 LoC bash
//! heredoc) to a Rust runner binary, mirroring the Wave 2 / Wave 3
//! pattern (ci-watcher-runner, planner-runner, verifier-dol-runner).
//! The role's `entrypoint_script` becomes a one-line shell wrapper that
//! execs `/usr/local/bin/shipper-runner`.
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! 1. Read startup bundle on stdin; extract `brief_id`, `target_repo`,
//!    `base_branch`, `pr_title`, `pr_body`, `forge_host` (defaults match
//!    bash `jq -r '... // "..."'` fall-throughs).
//! 2. Validate `GITEA_TOKEN` and `/workspace/.git` presence.
//! 3. `git config http.sslVerify false` + `user.{email,name}`.
//! 4. `git push` via `git -c http.extraheader="Authorization: token <T>"`
//!    to a CREDENTIAL-FREE https URL built from `forge_host` +
//!    `target_repo`. The token NEVER appears in the URL — git's stderr
//!    cannot leak it back. Push refspec is `HEAD:auto/<brief_id>` with
//!    `--force-with-lease` to handle rework iterations safely.
//!    On failure, `tail_stderr_scrubbed` redacts the token from any
//!    captured stderr before it lands in the emitted event (belt-and-
//!    suspenders against any unexpected leak path).
//! 5. POST `{forge_host}/api/v1/repos/{owner}/{repo_name}/pulls` via
//!    `curl` with `-H "Authorization: token <T>"` (NOT in URL); parse
//!    `html_url` + `number` from the JSON response.
//! 6. `git rev-parse HEAD` → `head_sha`.
//! 7. `emit_message` to `ci-watcher-agentry` with
//!    `{ pr_number, pr_url, head_sha }`.
//! 8. `emit_done shipped`.
//!
//! `DoneGuard` covers any unwound path so the daemon always sees a
//! terminal `done` event (EPIC #161 B0 invariant).
//!
//! ## Reviewer-claude v1 Blocker (lifted verbatim)
//!
//! v1 used `https://oauth2:TOKEN@host/repo.git` — captured stderr on a
//! push failure echoed the URL verbatim and leaked `GITEA_TOKEN` into
//! the structured event. v2 fixes this by using `-c http.extraheader`
//! (matching the bash heredoc's design) and a credential-free URL.

use std::process::{Command, Stdio};

use agentry_role_runtime::shipper_runner::{
    build_pr_create_body, classify_pre_push_rebase, compose_pr_body, git_fetch_argv, git_push_argv,
    parse_pr_response, parse_shipper_payload, push_url_credential_free, split_target_repo,
    tail_stderr_scrubbed, PrePushRebaseDecision, ShipperPayload,
};
use agentry_role_runtime::{
    emit_done, emit_event, emit_message, read_bundle_value, workspace_is_git_repo, DoneGuard,
};
use orchestrator_types::{DoneReason, EventVerdict};
use serde_json::{json, Value};

const WORKSPACE_DIR: &str = "/workspace";
/// `tail -c <N>` budget for captured `git push` stderr — matches the
/// bash heredoc's `tail -20 /tmp/push.err` shape (last 20 lines ~= 4 KiB
/// at typical line lengths; we cap by bytes here for predictability).
const PUSH_STDERR_TAIL: usize = 4096;

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
                }),
            );
            return;
        }
    };

    let payload = parse_shipper_payload(&bundle);
    let ShipperPayload {
        brief_id,
        target_repo,
        base_branch,
        pr_title,
        pr_body,
        forge_host,
        redeploy_required: _,
    } = &payload;
    let branch = format!("auto/{brief_id}");
    let (owner, repo_name) = split_target_repo(target_repo);

    let token = match std::env::var("GITEA_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            emit_event(json!({"error": "GITEA_TOKEN not in env"}));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    if !workspace_is_git_repo(WORKSPACE_DIR) {
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

    if let Err(e) = git_config(&[
        ("http.sslVerify", "false"),
        ("user.email", "shipper-agentry@agentry.lab"),
        ("user.name", "shipper-agentry"),
    ]) {
        emit_event(json!({
            "error": "git config failed",
            "detail": e,
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    let push_url = push_url_credential_free(forge_host, target_repo);

    // Pre-push fetch + rebase: catch develop drift between coder run and
    // shipper push. Eliminates the dominant race window that
    // pr-rebaser-agentry would otherwise have to recover from later
    // (cf. ci-watcher's chained pr-rebaser fallback for the
    // develop-advances-during-CI-poll case).
    emit_event(json!({
        "msg": "pre-push fetch",
        "base_branch": base_branch,
    }));
    let fetch_argv = git_fetch_argv(&token, &push_url, base_branch);
    let fetch_out = match Command::new("git")
        .args(&fetch_argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            emit_event(json!({
                "error": "pre-push fetch failed",
                "detail": format!("spawn git: {e}"),
            }));
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause: "pre-push fetch failed".into(),
                    exit_code: None,
                }),
            );
            return;
        }
    };
    if !fetch_out.status.success() {
        let detail = tail_stderr_scrubbed(&fetch_out.stderr, PUSH_STDERR_TAIL, &token);
        emit_event(json!({
            "error": "pre-push fetch failed",
            "detail": detail,
        }));
        emit_done(
            EventVerdict::Failed,
            Some(DoneReason {
                cause: "pre-push fetch failed".into(),
                exit_code: fetch_out.status.code(),
            }),
        );
        return;
    }

    emit_event(json!({"msg": "pre-push rebase on FETCH_HEAD"}));
    let rebase_out = match Command::new("git")
        .args(["rebase", "FETCH_HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            emit_event(json!({
                "error": "pre-push rebase spawn failed",
                "detail": format!("spawn git: {e}"),
            }));
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause: "pre-push rebase spawn failed".into(),
                    exit_code: None,
                }),
            );
            return;
        }
    };
    let rebase_rc = rebase_out.status.code().unwrap_or(-1);

    let status_porcelain = match Command::new("git")
        .args(["status", "--porcelain"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    };

    match classify_pre_push_rebase(rebase_rc, &status_porcelain) {
        PrePushRebaseDecision::Proceed => {}
        PrePushRebaseDecision::AbortConflict => {
            // Best-effort cleanup; ignore exit status.
            let _ = Command::new("git")
                .args(["rebase", "--abort"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let detail = tail_stderr_scrubbed(&rebase_out.stderr, PUSH_STDERR_TAIL, &token);
            emit_event(json!({
                "error": "pre-push rebase conflict",
                "detail": detail,
            }));
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause:
                        "pre-push rebase conflict — coder branch diverged from base unresolvably"
                            .into(),
                    exit_code: Some(rebase_rc),
                }),
            );
            return;
        }
        PrePushRebaseDecision::AbortFatal => {
            let _ = Command::new("git")
                .args(["rebase", "--abort"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let detail = tail_stderr_scrubbed(&rebase_out.stderr, PUSH_STDERR_TAIL, &token);
            emit_event(json!({
                "error": "pre-push rebase failed",
                "detail": detail,
            }));
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause: "pre-push rebase failed".into(),
                    exit_code: Some(rebase_rc),
                }),
            );
            return;
        }
    }

    emit_event(json!({
        "msg": "pushing branch",
        "branch": branch,
    }));
    let argv = git_push_argv(&token, &push_url, &branch);
    let push_out = match Command::new("git")
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            emit_event(json!({
                "error": "git push failed",
                "detail": format!("spawn git: {e}"),
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };
    if !push_out.status.success() {
        let detail = tail_stderr_scrubbed(&push_out.stderr, PUSH_STDERR_TAIL, &token);
        emit_event(json!({
            "error": "git push failed",
            "detail": detail,
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    emit_event(json!({
        "msg": "opening PR",
        "repo": target_repo,
        "head": branch,
    }));
    let composed_pr_body = compose_pr_body(pr_body, &payload.redeploy_required);
    let body = build_pr_create_body(pr_title, &composed_pr_body, &branch, base_branch);
    let pr_api_url = format!("https://{forge_host}/api/v1/repos/{owner}/{repo_name}/pulls");
    let pr_resp_text = match http_post_json(&pr_api_url, &token, &body.to_string()) {
        Ok(t) => t,
        Err(e) => {
            emit_event(json!({
                "error": "PR API call failed",
                "detail": e,
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };
    let pr_resp_json: Value = match serde_json::from_str(&pr_resp_text) {
        Ok(v) => v,
        Err(_) => {
            emit_event(json!({
                "error": "PR API call failed",
                "detail": pr_resp_text,
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };
    let pr = match parse_pr_response(&pr_resp_json) {
        Some(p) => p,
        None => {
            emit_event(json!({
                "error": "PR API call failed",
                "detail": pr_resp_text,
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    emit_event(json!({
        "msg": "PR opened",
        "url": pr.pr_url,
        "number": pr.pr_number,
    }));

    let head_sha = match git_rev_parse_head() {
        Ok(s) => s,
        Err(e) => {
            emit_event(json!({
                "error": "git rev-parse HEAD failed",
                "detail": e,
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    emit_message(
        "ci-watcher-agentry",
        json!({
            "pr_number": pr.pr_number,
            "pr_url": pr.pr_url,
            "head_sha": head_sha,
        }),
    );

    emit_done(EventVerdict::Shipped, None);
}

fn git_config(pairs: &[(&str, &str)]) -> Result<(), String> {
    for (key, value) in pairs {
        let out = Command::new("git")
            .args(["config", key, value])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("spawn git config: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git config {key} exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim(),
            ));
        }
    }
    Ok(())
}

fn git_rev_parse_head() -> Result<String, String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git rev-parse: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git rev-parse exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `curl -sS -k -X POST <url> -H "Authorization: token <T>" -H Content-Type
///       -d <body>`. Token is in the header argv, NEVER in the URL.
fn http_post_json(url: &str, token: &str, body: &str) -> Result<String, String> {
    let auth = format!("Authorization: token {token}");
    let out = Command::new("curl")
        .args([
            "-sS",
            "-k",
            "-X",
            "POST",
            url,
            "-H",
            &auth,
            "-H",
            "Content-Type: application/json",
            "-d",
            body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
