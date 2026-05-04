//! git-op-commit — substrate-side commit phase.
//!
//! Reads the brief bundle on stdin, runs `git config` + `git add -A` +
//! `git commit -m` against the workspace, and emits a terminal `done shipped`
//! NDJSON event carrying `{committed, sha, branch}`. The matching push +
//! PR-open work lives in `git-op-push`; this binary is the first half of the
//! split that brief 190b ships under #182.

use anyhow::Result;
use coder_precommit::git_operator::{
    auto_branch, emit_event, git_config_idempotent, read_bundle_from_stdin, stage_and_commit,
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
    let commit_message = bundle.brief.payload.commit_message.clone();
    let branch = auto_branch(&brief_id);

    git_config_idempotent(&workspace).await?;
    let sha = stage_and_commit(&workspace, &commit_message).await?;

    emit_event(&serde_json::json!({
        "type": "done",
        "verdict": "shipped",
        "committed": true,
        "sha": sha,
        "branch": branch
    }));
    guard.emitted = true;
    Ok(())
}
