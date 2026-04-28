//! git-operator — substrate-side publisher.
//!
//! Reads the brief bundle on stdin, runs git add/commit/push in /workspace,
//! opens a PR via gitea REST API. Emits NDJSON events on stdout. Always
//! emits a final `done` event before exiting.
//!
//! Per EPIC #161: Rust replaces the bash heredoc pattern. Always-emit-done
//! enforced by a Drop-guard so an unexpected panic still produces a
//! structured failed-done event, not the silent exit-5 class from #160.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::process::ExitCode;
use tokio::process::Command;

#[derive(Deserialize)]
struct Bundle {
    brief: Brief,
}

#[derive(Deserialize)]
struct Brief {
    id: String,
    payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
    target_repo: String,
    base_branch: String,
    pr_title: String,
    pr_body: String,
    #[serde(default = "default_commit_message")]
    commit_message: String,
    forge_host: Option<String>,
}

fn default_commit_message() -> String {
    "agentry: automated commit".into()
}

/// Drop guard: emits a `done failed` event if no terminal `done` was emitted
/// during the run. Guarantees the orchestrator never sees a silent exit
/// (the exit-5 class from #160).
struct DoneGuard {
    emitted: bool,
    brief_id: String,
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        if !self.emitted {
            emit_event(&serde_json::json!({
                "type": "done",
                "verdict": "failed",
                "reason": {"unexpected_exit": true, "brief": self.brief_id}
            }));
        }
    }
}

fn emit_event(payload: &serde_json::Value) {
    let line = serde_json::to_string(payload).unwrap_or_default();
    println!("{line}");
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            emit_event(&serde_json::json!({
                "type": "event",
                "payload": {"error": format!("{e:#}")}
            }));
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<()> {
    let mut bundle_str = String::new();
    use std::io::Read;
    std::io::stdin()
        .read_to_string(&mut bundle_str)
        .context("reading stdin bundle")?;
    let bundle: Bundle = serde_json::from_str(&bundle_str).context("parsing bundle JSON")?;
    let brief_id = bundle.brief.id.clone();
    let mut guard = DoneGuard {
        emitted: false,
        brief_id: brief_id.clone(),
    };

    let token = std::env::var("GITEA_TOKEN").context("GITEA_TOKEN env not set")?;
    let forge_host = bundle
        .brief
        .payload
        .forge_host
        .clone()
        .unwrap_or_else(|| "agency.lab:3000".into());
    let target_repo = bundle.brief.payload.target_repo.clone();
    let base_branch = bundle.brief.payload.base_branch.clone();
    let branch = format!("auto/{}", brief_id);
    let commit_message = bundle.brief.payload.commit_message.clone();
    let pr_title = bundle.brief.payload.pr_title.clone();
    let pr_body = bundle.brief.payload.pr_body.clone();

    // Workspace path defaults to /workspace (the production substrate mount).
    // Overridable via GIT_OPERATOR_WORKSPACE for hermetic smoke tests; the
    // role definition does NOT pass-through this env, so production always
    // uses /workspace.
    let workspace_str =
        std::env::var("GIT_OPERATOR_WORKSPACE").unwrap_or_else(|_| "/workspace".into());
    let workspace = std::path::Path::new(&workspace_str);
    if !workspace.join(".git").exists() {
        return Err(anyhow!(
            "no .git in {workspace_str} — coder did not produce a worktree"
        ));
    }

    // 1. git config (idempotent).
    run_git(
        workspace,
        &["config", "user.email", "git-operator@agentry.lab"],
    )
    .await?;
    run_git(workspace, &["config", "user.name", "git-operator"]).await?;
    run_git(workspace, &["config", "http.sslVerify", "false"]).await?;

    // 2. Stage + commit. If nothing to commit, fail loudly — caller broke
    // the contract.
    run_git(workspace, &["add", "-A"]).await?;
    let status = capture_git(workspace, &["status", "--porcelain"]).await?;
    if status.trim().is_empty() {
        return Err(anyhow!(
            "no changes to commit — caller invoked git-operator with a clean worktree"
        ));
    }
    run_git(workspace, &["commit", "-m", &commit_message]).await?;
    let sha = capture_git(workspace, &["rev-parse", "HEAD"]).await?;
    let sha = sha.trim().to_string();
    emit_event(&serde_json::json!({
        "type": "event",
        "payload": {"msg": "committed", "sha": sha, "branch": &branch}
    }));

    // 3. Push the branch.
    run_git(workspace, &["push", "-u", "origin", &branch]).await?;
    emit_event(&serde_json::json!({
        "type": "event",
        "payload": {"msg": "pushed", "branch": &branch}
    }));

    // 4. Open the PR via gitea REST API.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let url = format!("https://{forge_host}/api/v1/repos/{target_repo}/pulls");
    let payload = serde_json::json!({
        "title": pr_title,
        "body": pr_body,
        "head": branch,
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
    emit_event(&serde_json::json!({
        "type": "event",
        "payload": {"msg": "pr_opened", "pr_number": pr_number, "pr_url": pr_url}
    }));

    // 5. Terminal done.
    emit_event(&serde_json::json!({
        "type": "done",
        "verdict": "shipped",
        "pr_number": pr_number,
        "pr_url": pr_url,
        "sha": sha
    }));
    guard.emitted = true;
    Ok(())
}

async fn run_git(cwd: &std::path::Path, args: &[&str]) -> Result<()> {
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

async fn capture_git(cwd: &std::path::Path, args: &[&str]) -> Result<String> {
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
