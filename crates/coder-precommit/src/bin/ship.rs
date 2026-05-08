//! ship — runs the validator pipeline for the current brief and emits a
//! structured JSON report. Brief 6 of EPIC #152 makes this the only path
//! to publication; today the coder may still call git directly.

use anyhow::{Context, Result};
use clap::Parser;
use orchestrator_types::{TaskShape, ValidatorPipeline};
use std::path::PathBuf;
use tokio::process::Command;
use validators::{registry_for, BriefCtx, ValidatorReport};

#[derive(Parser)]
struct Args {
    /// Commit message for the eventual commit. Currently informational.
    #[arg(long)]
    commit_message: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _args = Args::parse();

    let brief_id = std::env::var("AGENTRY_BRIEF_ID")
        .context("AGENTRY_BRIEF_ID not set; ship must be invoked from a coder container")?;
    let shape = std::env::var("AGENTRY_BRIEF_KIND")
        .ok()
        .and_then(|s| serde_json::from_value::<TaskShape>(serde_json::Value::String(s)).ok())
        .unwrap_or(TaskShape::Feature);
    let pipeline: ValidatorPipeline = shape.into();
    let base_branch = std::env::var("AGENTRY_BASE_BRANCH").unwrap_or_else(|_| "develop".into());
    let workspace_path = PathBuf::from("/workspace");

    let changed_files = changed_files(&workspace_path, &base_branch)
        .await
        .unwrap_or_default();

    let ctx = BriefCtx {
        workspace_path,
        brief_id: brief_id.clone(),
        changed_files,
    };

    let validators_list = registry_for(pipeline);
    let mut tasks: tokio::task::JoinSet<Result<ValidatorReport>> = tokio::task::JoinSet::new();
    for v in validators_list {
        let ctx = ctx.clone();
        tasks.spawn(async move { v.run(&ctx).await });
    }

    let mut reports: Vec<ValidatorReport> = Vec::new();
    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(Ok(report)) => reports.push(report),
            Ok(Err(e)) => reports
                .push(ValidatorReport::fail("<dispatch>", vec![]).with_message(format!("{e}"))),
            Err(e) => {
                reports.push(ValidatorReport::fail("<panic>", vec![]).with_message(format!("{e}")))
            }
        }
    }

    let all_passed = reports.iter().all(|r| r.passed);

    reports.sort_by(|a, b| a.validator_name.cmp(&b.validator_name));

    let output = serde_json::json!({
        "ok": all_passed,
        "brief_id": brief_id,
        "kind": shape,
        "pipeline": pipeline,
        "validators": reports,
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

/// Changed-file paths relative to workspace root, derived from
/// `git diff --name-only origin/<base>...HEAD`.
async fn changed_files(workspace: &std::path::Path, base: &str) -> Result<Vec<PathBuf>> {
    let out = Command::new("git")
        .args(["diff", "--name-only", &format!("origin/{base}...HEAD")])
        .current_dir(workspace)
        .output()
        .await?;
    if !out.status.success() {
        return Ok(vec![]);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}
