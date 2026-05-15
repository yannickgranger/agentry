//! Pure helpers for the pr-rebaser-runner binary (EPIC #161 wave-bash port).
//! Lives in the lib crate so the test crate at
//! `crates/agentry-role-runtime/tests/pr_rebaser_runner_test.rs` can reach
//! them without touching the `src/bin/` file (the
//! `arch-ban-inline-cfg-test-in-src` rule forbids inline `#[cfg(test)]`
//! modules under `src/`).

use serde_json::Value;

use crate::pointer_str;

/// Parsed brief payload for the pr-rebaser-agentry role. Fields come from
/// the chained brief that ci-watcher emitted into the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaserPayload {
    pub target_repo: String,
    pub pr_number: i64,
    pub branch: String,
    pub base_branch: String,
    pub forge_host: String,
}

/// Reason a payload was rejected. The runner maps each to an error event
/// before going `done failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadError {
    MissingBranch,
}

/// Extract the rebaser inputs from a startup bundle JSON value.
///
/// Mirrors the bash:
/// ```sh
/// target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")
/// pr_number=$(jq -r '.brief.payload.pr_number // ""' <<<"$bundle")
/// branch=$(jq -r '.brief.payload.branch // ""' <<<"$bundle")
/// base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
/// forge_host=$(jq -r '.brief.payload.forge_host // "forge.example.com:3000"' <<<"$bundle")
/// ```
///
/// Unlike the bash original, `forge_host` has NO inline default — phase-3
/// daemon cascade through `agentry.toml [forge] default_host` is now the
/// single source of truth. An absent / empty value falls back to the
/// daemon-injected agentry default of `forge.example.com:3000` (kept as a defensive
/// fallback so the runner never panics on a malformed bundle, but the
/// observable contract is that the daemon always populates this field).
pub fn parse_rebaser_payload(bundle: &Value) -> Result<RebaserPayload, PayloadError> {
    let target_repo = match pointer_str(bundle, "/brief/payload/target_repo") {
        "" => "yg/agentry".to_string(),
        s => s.to_string(),
    };
    let pr_number = bundle
        .pointer("/brief/payload/pr_number")
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0);
    let branch = pointer_str(bundle, "/brief/payload/branch").to_string();
    let base_branch = match pointer_str(bundle, "/brief/payload/base_branch") {
        "" => "develop".to_string(),
        s => s.to_string(),
    };
    let forge_host = match pointer_str(bundle, "/brief/payload/forge_host") {
        "" => "forge.example.com:3000".to_string(),
        s => s.to_string(),
    };

    if branch.is_empty() {
        return Err(PayloadError::MissingBranch);
    }
    Ok(RebaserPayload {
        target_repo,
        pr_number,
        branch,
        base_branch,
        forge_host,
    })
}

/// Compose the token-bearing https remote URL the rebaser uses for `git
/// push --force-with-lease`. Mirrors the established forge pattern
/// (`https://oauth2:<token>@<host>/<owner>/<repo>.git`).
pub fn compose_remote_url(forge_host: &str, target_repo: &str, token: &str) -> String {
    format!("https://oauth2:{token}@{forge_host}/{target_repo}.git")
}

/// Parse `git status --porcelain=v2 -uno` output into the list of unmerged
/// paths. Mirrors the bash:
///
/// ```sh
/// printf '%s\n' "$status_out" | awk '/^u / {print $NF}'
/// ```
///
/// In porcelain v2, conflict entries start with `u ` (lowercase u) and the
/// LAST whitespace-delimited token is the path.
pub fn parse_unmerged_files(status_porcelain_v2: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in status_porcelain_v2.lines() {
        if let Some(rest) = line.strip_prefix("u ") {
            if let Some(path) = rest.split_whitespace().next_back() {
                out.push(path.to_string());
            }
        }
    }
    out
}

/// Build the argv for `git push --force-with-lease origin <branch>` so
/// tests can lock the command shape independently of how it's spawned.
pub fn push_force_with_lease_args(branch: &str) -> Vec<String> {
    vec![
        "push".to_string(),
        "--force-with-lease".to_string(),
        "origin".to_string(),
        branch.to_string(),
    ]
}

/// Classify `git rebase` exit. `Success` only when exit code is 0.
/// Anything else is a failure — the caller separately inspects
/// `git status --porcelain=v2 -uno` to decide between conflict (Rework)
/// and non-conflict failure (Fatal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseOutcome {
    Success,
    Conflict,
    Fatal,
}

/// Decide rebase outcome from the rebase exit code and porcelain status.
pub fn classify_rebase(exit_code: i32, status_porcelain_v2: &str) -> RebaseOutcome {
    if exit_code == 0 {
        return RebaseOutcome::Success;
    }
    if !parse_unmerged_files(status_porcelain_v2).is_empty() {
        RebaseOutcome::Conflict
    } else {
        RebaseOutcome::Fatal
    }
}
