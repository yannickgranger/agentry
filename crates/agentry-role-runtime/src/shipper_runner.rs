//! Pure helpers for the `shipper-runner` binary (EPIC #161 Wave-bash port
//! of `SHIPPER_AGENTRY_SCRIPT`). Extracted to a lib module so the
//! `tests/shipper_runner_test.rs` test crate can exercise them — the
//! workspace's `arch-ban-inline-cfg-test-in-src.cypher` rule (PR #295)
//! forbids inline `#[cfg(test)] mod tests` blocks in `src/`.
//!
//! Security note (v1 reviewer Blocker): tokens never appear in URLs.
//! [`push_url_credential_free`] returns a token-free URL; auth is injected
//! via git's `-c http.extraheader` (see [`git_push_argv`]). On push
//! failure, [`tail_stderr_scrubbed`] redacts the token from any captured
//! stderr before it lands in an emitted event.

use serde_json::{json, Value};

use crate::tail_bytes;

/// Build the credential-free https URL for `git push` / `git fetch`.
///
/// Returns `https://<forge_host>/<target_repo>.git` with NO embedded
/// credentials (no `oauth2:TOKEN@` prefix, no `?token=` query). Auth is
/// passed to git via [`git_push_argv`]'s `-c http.extraheader` so the
/// token never lands in the URL — git's stderr cannot leak it back when
/// a push fails.
pub fn push_url_credential_free(forge_host: &str, target_repo: &str) -> String {
    format!("https://{forge_host}/{target_repo}.git")
}

/// Build the argv for the authenticated `git push`.
///
/// Verbatim port of the bash heredoc's mechanism:
/// `git -c http.sslVerify=false -c http.extraheader="Authorization: token <T>"
///      push <credential_free_url> HEAD:<branch> --force-with-lease`.
///
/// The token lives in the argv vector (via `-c http.extraheader=...`),
/// NEVER in the URL — even if git's stderr echoes the URL on failure,
/// the token is not in it.
pub fn git_push_argv(token: &str, push_url: &str, branch: &str) -> Vec<String> {
    vec![
        "-c".to_string(),
        "http.sslVerify=false".to_string(),
        "-c".to_string(),
        format!("http.extraheader=Authorization: token {token}"),
        "push".to_string(),
        push_url.to_string(),
        format!("HEAD:{branch}"),
        "--force-with-lease".to_string(),
    ]
}

/// Build the argv for the authenticated pre-push `git fetch <base_branch>`.
///
/// Same auth mechanism as [`git_push_argv`]: token lives in
/// `-c http.extraheader=Authorization: token <T>`, NEVER in the URL.
/// `fetch_url` is the credential-free https URL produced by
/// [`push_url_credential_free`].
pub fn git_fetch_argv(token: &str, fetch_url: &str, base_branch: &str) -> Vec<String> {
    vec![
        "-c".to_string(),
        "http.sslVerify=false".to_string(),
        "-c".to_string(),
        format!("http.extraheader=Authorization: token {token}"),
        "fetch".to_string(),
        fetch_url.to_string(),
        base_branch.to_string(),
    ]
}

/// Decision the shipper-runner reaches for the pre-push rebase given the
/// rebase exit code and `git status --porcelain` output. `Proceed` only
/// when the rebase exited cleanly. `AbortConflict` covers the dominant
/// failure (non-zero exit + unmerged paths — coder branch diverged
/// unresolvably from the freshly fetched base). `AbortFatal` covers any
/// other non-zero exit (spawn failure inside git, missing FETCH_HEAD,
/// etc.) — distinct from conflict so the emitted cause is accurate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrePushRebaseDecision {
    Proceed,
    AbortConflict,
    AbortFatal,
}

/// Classify the pre-push rebase outcome from the rebase exit code and
/// `git status --porcelain` (v1) output.
///
/// `exit_code == 0` → [`PrePushRebaseDecision::Proceed`]. Non-zero with
/// any unmerged-path code in the porcelain output (`UU`, `AA`, `DD`,
/// `AU`, `UA`, `UD`, `DU`) → [`PrePushRebaseDecision::AbortConflict`].
/// Non-zero with no unmerged paths → [`PrePushRebaseDecision::AbortFatal`].
pub fn classify_pre_push_rebase(exit_code: i32, status_porcelain: &str) -> PrePushRebaseDecision {
    if exit_code == 0 {
        return PrePushRebaseDecision::Proceed;
    }
    if porcelain_v1_has_unmerged(status_porcelain) {
        PrePushRebaseDecision::AbortConflict
    } else {
        PrePushRebaseDecision::AbortFatal
    }
}

fn porcelain_v1_has_unmerged(status_porcelain: &str) -> bool {
    status_porcelain.lines().any(|line| {
        let bytes = line.as_bytes();
        if bytes.len() < 2 {
            return false;
        }
        matches!(
            &bytes[..2],
            b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU"
        )
    })
}

/// Replace every occurrence of `token` in `text` with `[REDACTED]`.
/// No-op when `token` is empty.
///
/// Belt-and-suspenders defence: even though [`git_push_argv`] keeps the
/// token out of every URL it builds, this scrub guards against any
/// unexpected leak path (env-var echo, future code drift, third-party
/// tool output).
pub fn scrub_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "[REDACTED]")
}

/// Last `n` bytes of `buf` as a lossy UTF-8 string with `token` scrubbed
/// to `[REDACTED]`. Use this on any captured stderr (push, fetch, etc.)
/// before including it in an emitted event payload.
pub fn tail_stderr_scrubbed(buf: &[u8], n: usize, token: &str) -> String {
    let tailed = tail_bytes(buf, n);
    scrub_token(&tailed, token)
}

/// Build the JSON body for the forge `POST /repos/.../pulls` call.
/// Mirrors bash:
/// ```sh
/// jq -n --arg t "$pr_title" --arg b "$pr_body" --arg h "$branch" --arg base "$base_branch" \
///     '{title:$t,body:$b,head:$h,base:$base}'
/// ```
pub fn build_pr_create_body(title: &str, body: &str, head: &str, base: &str) -> Value {
    json!({
        "title": title,
        "body": body,
        "head": head,
        "base": base,
    })
}

/// Parsed PR-creation response: the bits the shipper forwards to
/// `ci-watcher-agentry` via `emit_message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCreateResponse {
    pub pr_number: i64,
    pub pr_url: String,
}

/// Parse the forge's pull-create JSON response. Mirrors bash
/// `jq -r '.html_url // ""'` + `jq -r '.number // 0'`.
///
/// Returns `None` when `html_url` is missing/empty/null (the bash
/// `[ -z "$pr_url" ] || [ "$pr_url" = "null" ]` failure check).
pub fn parse_pr_response(resp: &Value) -> Option<PrCreateResponse> {
    let pr_url = resp.get("html_url").and_then(Value::as_str)?;
    if pr_url.is_empty() {
        return None;
    }
    let pr_number = resp.get("number").and_then(Value::as_i64).unwrap_or(0);
    Some(PrCreateResponse {
        pr_number,
        pr_url: pr_url.to_string(),
    })
}

/// Parsed brief payload as consumed by the shipper runner. Defaults
/// mirror the bash `jq -r '... // "..."'` fall-through values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShipperPayload {
    pub brief_id: String,
    pub target_repo: String,
    pub base_branch: String,
    pub pr_title: String,
    pub pr_body: String,
    pub forge_host: String,
}

/// Extract the shipper inputs from a startup bundle JSON value. Mirrors:
/// ```sh
/// brief_id=$(jq -r '.brief.id' <<<"$bundle")
/// target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")
/// base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
/// pr_title=$(jq -r '.brief.payload.pr_title // ("auto(" + .brief.id + ")")' <<<"$bundle")
/// pr_body=$(jq -r '.brief.payload.pr_body // "Agentry-produced PR. ..."' <<<"$bundle")
/// forge_host=$(jq -r '.brief.payload.forge_host // "agency.lab:3000"' <<<"$bundle")
/// ```
pub fn parse_shipper_payload(bundle: &Value) -> ShipperPayload {
    let brief_id = crate::pointer_str(bundle, "/brief/id").to_string();
    let target_repo = crate::pointer_str_or(bundle, "/brief/payload/target_repo", "yg/agentry");
    let base_branch = crate::pointer_str_or(bundle, "/brief/payload/base_branch", "develop");
    let default_title = format!("auto({brief_id})");
    let pr_title = crate::pointer_str_or(bundle, "/brief/payload/pr_title", &default_title);
    let pr_body = crate::pointer_str_or(
        bundle,
        "/brief/payload/pr_body",
        "Agentry-produced PR. See brief trace stream.",
    );
    let forge_host = crate::pointer_str_or(bundle, "/brief/payload/forge_host", "agency.lab:3000");
    ShipperPayload {
        brief_id,
        target_repo,
        base_branch,
        pr_title,
        pr_body,
        forge_host,
    }
}

/// Split `target_repo` (`"owner/repo"`) into `(owner, repo)`. Empty
/// strings on either side when the slash is missing.
pub fn split_target_repo(target_repo: &str) -> (String, String) {
    let mut parts = target_repo.splitn(2, '/');
    let owner = parts.next().unwrap_or("").to_string();
    let repo_name = parts.next().unwrap_or("").to_string();
    (owner, repo_name)
}
