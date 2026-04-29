//! git-op-rebase — substrate-side rebase phase.
//!
//! Reads the brief bundle on stdin, fetches `base_branch` from origin and
//! rebases the workspace HEAD onto it. Emits a terminal `done shipped`
//! NDJSON event carrying `{rebased, base_branch}` on success. On rebase
//! conflict the worktree is left clean (via `git rebase --abort`) and the
//! binary exits 1; the DoneGuard then emits `done failed` with
//! `unexpected_exit`. Brief 192b will refine the failure path to a
//! structured `done rework_needed` once #191's loop-back vocabulary lands.

use anyhow::Result;
use coder_precommit::git_operator::{
    emit_event, read_bundle_from_stdin, rebase_onto, workspace_path, DoneGuard,
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
    let base_branch = bundle.brief.payload.base_branch.clone();

    rebase_onto(&workspace, &base_branch).await?;

    emit_event(&serde_json::json!({
        "type": "done",
        "verdict": "shipped",
        "rebased": true,
        "base_branch": base_branch
    }));
    guard.emitted = true;
    Ok(())
}
