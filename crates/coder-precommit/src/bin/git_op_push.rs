//! git-op-push — substrate-side push + PR-open phase.
//!
//! Reads the brief bundle on stdin, pushes the auto-branch (assumed already
//! committed by an upstream `git-op-commit` step), and opens a PR via the
//! gitea REST API. Emits a terminal `done shipped` NDJSON event carrying
//! `{pushed, pr_number, pr_url}`. Counterpart to `git-op-commit`.

use anyhow::{Context, Result};
use coder_precommit::git_operator::{
    auto_branch, emit_event, open_pull_request, push_branch, read_bundle_from_stdin,
    workspace_path, DoneGuard,
};
use std::process::ExitCode;

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
    let mut guard = DoneGuard {
        emitted: false,
        brief_id: "unknown".to_string(),
    };
    let bundle = read_bundle_from_stdin()?;
    let brief_id = bundle.brief.id.clone();
    guard.brief_id = brief_id.clone();

    let workspace = workspace_path()?;
    let token = std::env::var("GITEA_TOKEN").context("GITEA_TOKEN env not set")?;
    let forge_host = bundle
        .brief
        .payload
        .forge_host
        .clone()
        .unwrap_or_else(|| "agency.lab:3000".into());
    let target_repo = bundle.brief.payload.target_repo.clone();
    let base_branch = bundle.brief.payload.base_branch.clone();
    let pr_title = bundle.brief.payload.pr_title.clone();
    let pr_body = bundle.brief.payload.pr_body.clone();
    let branch = auto_branch(&brief_id);

    push_branch(&workspace, &branch).await?;
    let pr = open_pull_request(
        &forge_host,
        &target_repo,
        &branch,
        &base_branch,
        &pr_title,
        &pr_body,
        &token,
    )
    .await?;

    emit_event(&serde_json::json!({
        "type": "done",
        "verdict": "shipped",
        "pushed": true,
        "pr_number": pr.pr_number,
        "pr_url": pr.pr_url
    }));
    guard.emitted = true;
    Ok(())
}
