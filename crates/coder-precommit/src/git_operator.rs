//! Shared plumbing for the git-operator family of binaries.
//!
//! Brief 190b of EPIC #182 split the combined `git-operator` binary into a
//! commit-phase and a push-phase binary so the v1 self-host workflow can run
//! a reviewer between the two. The phase logic — bundle parsing, the
//! always-emit-done Drop guard, git plumbing, and the gitea PR call — lives
//! here so each thin binary stays ~30 lines and the legacy combined binary
//! preserves its external behavior unchanged.
//!
//! Per EPIC #161: NO bash logic — every step is Rust. Per #160: every exit
//! path emits a terminal `done` event, enforced by `DoneGuard::Drop`.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, Deserialize)]
pub struct Bundle {
    pub brief: Brief,
}

#[derive(Debug, Deserialize)]
pub struct Brief {
    pub id: String,
    pub payload: Payload,
}

#[derive(Debug, Deserialize)]
pub struct Payload {
    pub target_repo: String,
    pub base_branch: String,
    pub pr_title: String,
    pub pr_body: String,
    #[serde(default = "default_commit_message")]
    pub commit_message: String,
    pub forge_host: Option<String>,
}

pub fn default_commit_message() -> String {
    "agentry: automated commit".into()
}

/// Drop guard: emits a `done failed` event if no terminal `done` was emitted
/// during the run. Guarantees the orchestrator never sees a silent exit
/// (the exit-5 class from #160).
pub struct DoneGuard {
    pub emitted: bool,
    pub brief_id: String,
}

impl DoneGuard {
    /// Writer-based seam for the Drop emission. When `emitted` is false this
    /// writes the same `done failed` line that `Drop` would write to stdout;
    /// when `emitted` is true it writes nothing. Exposed so external tests
    /// can observe the Drop behavior without redirecting stdout and without
    /// needing a `#[cfg(test)]` hook in `src/`.
    pub fn emit_drop_event_to<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        if !self.emitted {
            emit_event_to(
                writer,
                &serde_json::json!({
                    "type": "done",
                    "verdict": "failed",
                    "reason": {"unexpected_exit": true, "brief": self.brief_id}
                }),
            )?;
        }
        Ok(())
    }
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.emit_drop_event_to(&mut std::io::stdout().lock());
    }
}

pub fn emit_event(payload: &serde_json::Value) {
    let _ = emit_event_to(&mut std::io::stdout().lock(), payload);
}

/// Writer-based seam for `emit_event`. Injects an `at` RFC3339 timestamp
/// when the payload lacks one, then writes the JSON line followed by `\n`.
/// `emit_event` is a thin wrapper that targets stdout.
pub fn emit_event_to<W: Write>(writer: &mut W, payload: &serde_json::Value) -> std::io::Result<()> {
    let mut value = payload.clone();
    if let Some(obj) = value.as_object_mut() {
        if !obj.contains_key("at") {
            obj.insert(
                "at".into(),
                serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
            );
        }
    }
    let line = serde_json::to_string(&value).unwrap_or_default();
    writeln!(writer, "{line}")
}

pub async fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("git {args:?}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

pub async fn capture_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("git {args:?}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8(out.stdout)?)
}

pub fn read_bundle_from_stdin() -> Result<Bundle> {
    use std::io::Read;
    let mut bundle_str = String::new();
    std::io::stdin()
        .read_to_string(&mut bundle_str)
        .context("reading stdin bundle")?;
    let bundle: Bundle = serde_json::from_str(&bundle_str).context("parsing bundle JSON")?;
    Ok(bundle)
}

/// Resolve the workspace path from `GIT_OPERATOR_WORKSPACE` (default
/// `/workspace`) and verify it contains a `.git` worktree. Returns Err
/// otherwise.
pub fn workspace_path() -> Result<PathBuf> {
    let s = std::env::var("GIT_OPERATOR_WORKSPACE").unwrap_or_else(|_| "/workspace".into());
    let p = PathBuf::from(&s);
    if !p.join(".git").exists() {
        return Err(anyhow!("no .git in {s} — coder did not produce a worktree"));
    }
    Ok(p)
}

pub fn auto_branch(brief_id: &str) -> String {
    format!("auto/{brief_id}")
}

pub async fn git_config_idempotent(cwd: &Path) -> Result<()> {
    run_git(cwd, &["config", "user.email", "git-operator@agentry.lab"]).await?;
    run_git(cwd, &["config", "user.name", "git-operator"]).await?;
    run_git(cwd, &["config", "http.sslVerify", "false"]).await?;
    Ok(())
}

/// Stage everything under `cwd`, fail loudly on a clean worktree, commit
/// with `commit_message`, and return the new HEAD sha (trimmed).
pub async fn stage_and_commit(cwd: &Path, commit_message: &str) -> Result<String> {
    run_git(cwd, &["add", "-A"]).await?;
    let status = capture_git(cwd, &["status", "--porcelain"]).await?;
    if status.trim().is_empty() {
        return Err(anyhow!(
            "no changes to commit — caller invoked git-operator with a clean worktree"
        ));
    }
    run_git(cwd, &["commit", "-m", commit_message]).await?;
    let sha = capture_git(cwd, &["rev-parse", "HEAD"]).await?;
    Ok(sha.trim().to_string())
}

pub async fn push_branch(cwd: &Path, branch: &str) -> Result<()> {
    run_git(cwd, &["push", "-u", "origin", branch]).await
}

/// Fetch the latest base_branch from origin, then rebase HEAD onto it.
/// Returns Ok(()) on a clean rebase. On conflict (any merge conflict markers
/// produced by `git rebase`), abort the rebase via `git rebase --abort` and
/// return Err with a structured detail (file paths reported by `git status`,
/// truncated to a sane size). The caller is responsible for emitting the
/// terminal `done` event with the failure reason.
pub async fn rebase_onto(cwd: &Path, base_branch: &str) -> Result<()> {
    run_git(cwd, &["fetch", "origin", base_branch]).await?;
    let rebase_target = format!("origin/{base_branch}");
    let out = Command::new("git")
        .args(["rebase", &rebase_target])
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("git rebase {rebase_target}"))?;
    if out.status.success() {
        return Ok(());
    }
    let conflicts = capture_git(cwd, &["diff", "--name-only", "--diff-filter=U"])
        .await
        .unwrap_or_default();
    let files: Vec<&str> = conflicts
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let count = files.len();
    let mut paths = files.join(", ");
    if paths.len() > 1024 {
        paths.truncate(1024);
        paths.push_str("...(truncated)");
    }
    let _ = run_git(cwd, &["rebase", "--abort"]).await;
    Err(anyhow!("rebase conflict in {count} file(s): {paths}"))
}

pub struct PrOpened {
    pub pr_number: u64,
    pub pr_url: String,
}

pub async fn open_pull_request(
    forge_host: &str,
    target_repo: &str,
    head_branch: &str,
    base_branch: &str,
    pr_title: &str,
    pr_body: &str,
    token: &str,
) -> Result<PrOpened> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("https://{forge_host}/api/v1/repos/{target_repo}/pulls");
    let payload = serde_json::json!({
        "title": pr_title,
        "body": pr_body,
        "head": head_branch,
        "base": base_branch,
    });
    let resp = client
        .post(&url)
        .header("Authorization", format!("token {token}"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;
    let status_code = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if !status_code.is_success() {
        return Err(anyhow!("gitea PR open failed: {status_code} — {body_text}"));
    }
    let pr_resp: serde_json::Value =
        serde_json::from_str(&body_text).context("parse PR response JSON")?;
    let pr_number = pr_resp.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
    let pr_url = pr_resp
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(PrOpened { pr_number, pr_url })
}
