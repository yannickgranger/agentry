//! ci-watcher-runner — full role lifecycle for ci-watcher-agentry.
//!
//! EPIC #161 Wave 2 ports `CI_WATCHER_AGENTRY_SCRIPT` (~200 LoC bash
//! heredoc) to a Rust runner binary, mirroring the Wave 2 auditor pattern.
//! The role's `entrypoint_script` becomes a one-line shell wrapper that
//! execs `/usr/local/bin/ci-watcher-runner`.
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! Read startup bundle on stdin; extract `brief.id`, `target_repo`,
//! `forge_host`. Locate the LAST shipper-agentry message in
//! `team_context.messages` via [`find_shipper_message`]; pull `pr_number`,
//! `head_sha`, `pr_url`. Validate `GITEA_TOKEN` and the shipper-routed
//! payload.
//!
//! Poll loop (max 120 × 15s) does two GETs per iteration:
//!
//! - `pulls/<num>`: `state=="closed"` → `done shipped` (PR merged or
//!   replaced externally). `mergeable==false` → write
//!   `/workspace/pr_rebaser_brief.json`, emit `_chain_trigger` with
//!   `next_brief_refs`, `done shipped`.
//! - `commits/<sha>/status`: `success` → POST merge with up to 6 retries
//!   on transient 405/409 (10×attempt seconds backoff capped at 60, plus
//!   0..=9s jitter via [`rand_jitter`]); `failure|error` → emit Blocker
//!   finding with first failing context (via [`first_failing_context`])
//!   and `done rework_needed`; `pending|unknown|""` → sleep 15s and
//!   re-poll; any other state → `done failed`.
//!
//! Loop exhausted → `done failed`.
//!
//! `DoneGuard` covers any unwound path so the daemon always sees a
//! terminal `done` event (EPIC #161 B0 invariant).

use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use agentry_role_runtime::ci_watcher_runner::{
    find_shipper_message, first_failing_context, rand_jitter,
};
use agentry_role_runtime::{
    emit_done, emit_event, emit_finding, emit_message, mech_finding, pointer_str_or,
    read_bundle_value, DoneGuard,
};
use chrono::Utc;
use orchestrator_types::{DoneReason, EventVerdict};
use serde_json::{json, Value};

const HOST_WORKSPACE_PREFIX: &str = "/var/mnt/workspaces/agentry-work/briefs";
const MAX_POLLS: u32 = 120;
const POLL_SLEEP_SECS: u64 = 15;
const MERGE_MAX_RETRIES: u32 = 6;
const MERGE_BACKOFF_CAP_SECS: u64 = 60;

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

    let brief_id = pointer_str_or(&bundle, "/brief/id", "");
    let target_repo = pointer_str_or(&bundle, "/brief/payload/target_repo", "yg/agentry");
    let forge_host = pointer_str_or(&bundle, "/brief/payload/forge_host", "agency.lab:3000");
    let (owner, repo_name) = split_target_repo(&target_repo);
    let host_workspace = format!("{HOST_WORKSPACE_PREFIX}/{brief_id}");

    let messages = bundle
        .pointer("/team_context/messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let shipper_msg = match find_shipper_message(&messages) {
        Some(m) => m.clone(),
        None => {
            emit_event(json!({
                "error": "no shipper-agentry message in team_context — cannot locate PR to watch",
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    let pr_number = match shipper_msg.pointer("/payload/pr_number") {
        Some(v) if v.is_u64() || v.is_i64() => v.as_i64().unwrap_or(0),
        _ => 0,
    };
    let head_sha = shipper_msg
        .pointer("/payload/head_sha")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let pr_url = shipper_msg
        .pointer("/payload/pr_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if pr_number <= 0 || head_sha.is_empty() {
        emit_event(json!({
            "error": "shipper message missing pr_number or head_sha",
            "detail": shipper_msg.to_string(),
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    emit_event(json!({
        "msg": "ci-watcher starting",
        "pr_number": pr_number,
        "head_sha": head_sha,
        "pr_url": pr_url,
    }));

    let token = match std::env::var("GITEA_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            emit_event(json!({"error": "GITEA_TOKEN not in env"}));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    let pr_url_api =
        format!("https://{forge_host}/api/v1/repos/{owner}/{repo_name}/pulls/{pr_number}");
    let status_url =
        format!("https://{forge_host}/api/v1/repos/{owner}/{repo_name}/commits/{head_sha}/status");

    for i in 1..=MAX_POLLS {
        // ---- Mergeable check (Brief 137b) ----
        let pr_resp = match http_get_json(&pr_url_api, &token) {
            Ok(v) => v,
            Err(e) => {
                emit_event(json!({"error": "pr GET failed", "detail": e}));
                sleep_secs(POLL_SLEEP_SECS);
                continue;
            }
        };
        let pr_state = pr_resp
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("open")
            .to_string();
        let pr_mergeable_raw = pr_resp.get("mergeable");
        let pr_branch = pr_resp
            .pointer("/head/ref")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let pr_base = pr_resp
            .pointer("/base/ref")
            .and_then(Value::as_str)
            .unwrap_or("develop")
            .to_string();

        if pr_state == "closed" {
            emit_event(json!({
                "msg": "pr closed externally",
                "pr_number": pr_number,
            }));
            emit_done(EventVerdict::Shipped, None);
            return;
        }

        // Bash treats only literal `false` as "develop moved under us";
        // `null` (still computing) falls through to the CI poll.
        if matches!(pr_mergeable_raw, Some(Value::Bool(false))) {
            if pr_branch.is_empty() {
                emit_event(json!({
                    "error": "pr_resp missing .head.ref — cannot chain-trigger rebaser without branch",
                }));
                emit_done(EventVerdict::Failed, None);
                return;
            }
            let rebaser_path = "/workspace/pr_rebaser_brief.json";
            let rebaser_brief_id = format!("brf_rebaser_{brief_id}_pr{pr_number}");
            let submitted_at = Utc::now().to_rfc3339();
            let child = json!({
                "id": rebaser_brief_id,
                "project": Value::Null,
                "topology": {"name": "agentry-pr-rebaser-v0", "version": 1},
                "payload": {
                    "target_repo": target_repo,
                    "pr_number": pr_number,
                    "branch": pr_branch,
                    "base_branch": pr_base,
                    "forge_host": forge_host,
                },
                "budget": {"max_wall_seconds": 600},
                "escalation": "autonomous",
                "parent_brief": brief_id,
                "submitted_by": format!("ci-watcher-agentry-{brief_id}"),
                "submitted_at": submitted_at,
            });
            if let Err(e) = std::fs::write(rebaser_path, child.to_string()) {
                emit_event(json!({
                    "error": "failed to write pr_rebaser_brief.json",
                    "detail": e.to_string(),
                }));
                emit_done(EventVerdict::Failed, None);
                return;
            }
            let host_path = format!("{host_workspace}/pr_rebaser_brief.json");
            emit_event(json!({
                "msg": "pr not mergeable — chain-triggering pr-rebaser-agentry",
                "pr_number": pr_number,
                "branch": pr_branch,
                "base_branch": pr_base,
                "next_brief_ref": host_path,
            }));
            emit_message("_chain_trigger", json!({"next_brief_refs": [host_path]}));
            emit_done(EventVerdict::Shipped, None);
            return;
        }

        // ---- CI status poll ----
        let resp = match http_get_json(&status_url, &token) {
            Ok(v) => v,
            Err(e) => {
                emit_event(json!({"error": "status GET failed", "detail": e}));
                sleep_secs(POLL_SLEEP_SECS);
                continue;
            }
        };
        let state = resp
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        emit_event(json!({
            "msg": "polling CI",
            "state": state,
            "iteration": i,
        }));
        match state.as_str() {
            "success" => {
                let merge_url = format!(
                    "https://{forge_host}/api/v1/repos/{owner}/{repo_name}/pulls/{pr_number}/merge"
                );
                merge_with_retry(&merge_url, &token, pr_number, &pr_url);
                return;
            }
            "failure" | "error" => {
                let statuses = resp
                    .get("statuses")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let ctx =
                    first_failing_context(&statuses).unwrap_or_else(|| "(no context)".to_string());
                emit_event(json!({
                    "msg": "CI red — emitting rework_needed for coder loop-back",
                    "state": state,
                    "failing_context": ctx,
                }));
                emit_finding(&mech_finding(
                    "ci-watcher",
                    "ci",
                    &format!("CI red on {ctx}"),
                ));
                emit_done(EventVerdict::ReworkNeeded, None);
                return;
            }
            "pending" | "unknown" | "" => {
                sleep_secs(POLL_SLEEP_SECS);
            }
            other => {
                emit_event(json!({
                    "error": "unexpected CI state",
                    "state": other,
                }));
                emit_done(EventVerdict::Failed, None);
                return;
            }
        }
    }

    emit_event(json!({
        "error": "CI poll exhausted 30min without success — giving up",
    }));
    emit_done(EventVerdict::Failed, None);
}

fn split_target_repo(target_repo: &str) -> (String, String) {
    let mut parts = target_repo.splitn(2, '/');
    let owner = parts.next().unwrap_or("").to_string();
    let repo_name = parts.next().unwrap_or("").to_string();
    (owner, repo_name)
}

/// `curl -sS -k -H "Authorization: token <T>" <URL>` and parse stdout as
/// JSON. Returns the spawn-error / non-zero-exit / parse-error message on
/// `Err`. Mirrors the bash `curl ... 2>/tmp/foo.err || { ...; sleep 15;
/// continue; }` shape — the caller decides whether to retry.
fn http_get_json(url: &str, token: &str) -> Result<Value, String> {
    let auth = format!("Authorization: token {token}");
    let out = Command::new("curl")
        .args(["-sS", "-k", "-H", &auth, url])
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
    serde_json::from_slice(&out.stdout).map_err(|e| format!("parse json: {e}"))
}

/// `curl -sS -k -X POST -H ... -d <body> -o <body_file> -w '%{http_code}'`.
/// Returns `(http_code, body_text)`. The bash version uses the same
/// `-o /tmp/merge.body -w '%{http_code}'` trick to capture both
/// independently.
fn http_post(url: &str, token: &str, body: &str) -> Result<(String, String), String> {
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
            "-w",
            "%{http_code}",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn curl: {e}"))?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(format!(
            "curl exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    let combined = String::from_utf8_lossy(&out.stdout).into_owned();
    // The HTTP code is the last 3 characters because `-w '%{http_code}'`
    // appends it after the response body. Defensive: if the response
    // body is shorter than 3 chars (rare but possible on empty 204),
    // fall back to the whole output as the code with an empty body.
    if combined.len() < 3 {
        return Ok((combined, String::new()));
    }
    let split_at = combined.len() - 3;
    let body = combined[..split_at].to_string();
    let code = combined[split_at..].to_string();
    Ok((code, body))
}

/// POST `{"Do":"merge"}` to the merge URL, retrying transient 405/409 up
/// to `MERGE_MAX_RETRIES` times with `10*attempt`-sec backoff capped at
/// 60s plus `rand_jitter()` jitter. Emits `done shipped` on 200/204,
/// `done failed` on retry-budget exhaustion or non-transient errors.
fn merge_with_retry(merge_url: &str, token: &str, pr_number: i64, pr_url: &str) {
    let body = r#"{"Do":"merge"}"#;
    let mut last_code = String::new();
    let mut last_detail = String::new();

    for attempt in 1..=MERGE_MAX_RETRIES {
        match http_post(merge_url, token, body) {
            Ok((code, detail)) => {
                last_code = code.clone();
                last_detail = detail.clone();
                if code == "200" || code == "204" {
                    emit_event(json!({
                        "msg": "merged",
                        "pr_number": pr_number,
                        "pr_url": pr_url,
                        "merge_attempt": attempt,
                    }));
                    emit_done(EventVerdict::Shipped, None);
                    return;
                }
                if code == "405" || code == "409" {
                    if attempt < MERGE_MAX_RETRIES {
                        let backoff = (10u64 * attempt as u64).min(MERGE_BACKOFF_CAP_SECS);
                        let sleep_seconds = backoff + rand_jitter();
                        emit_event(json!({
                            "msg": "merge transient failure — retrying",
                            "http_code": code,
                            "detail": detail,
                            "merge_attempt": attempt,
                            "sleep_seconds": sleep_seconds,
                        }));
                        sleep_secs(sleep_seconds);
                        continue;
                    }
                    break;
                }
                emit_event(json!({
                    "error": "merge API call failed (non-transient)",
                    "http_code": code,
                    "detail": detail,
                    "merge_attempt": attempt,
                }));
                emit_done(EventVerdict::Failed, None);
                return;
            }
            Err(e) => {
                last_detail = e;
                last_code = "(spawn-error)".into();
                if attempt < MERGE_MAX_RETRIES {
                    let backoff = (10u64 * attempt as u64).min(MERGE_BACKOFF_CAP_SECS);
                    let sleep_seconds = backoff + rand_jitter();
                    emit_event(json!({
                        "msg": "merge transient failure — retrying",
                        "http_code": last_code,
                        "detail": last_detail,
                        "merge_attempt": attempt,
                        "sleep_seconds": sleep_seconds,
                    }));
                    sleep_secs(sleep_seconds);
                    continue;
                }
                break;
            }
        }
    }

    emit_event(json!({
        "error": "merge retry budget exhausted (transient)",
        "http_code": last_code,
        "detail": last_detail,
        "merge_attempt": MERGE_MAX_RETRIES,
        "merge_max_retries": MERGE_MAX_RETRIES,
    }));
    emit_done(EventVerdict::Failed, None);
}

fn sleep_secs(secs: u64) {
    thread::sleep(Duration::from_secs(secs));
}
