use std::path::{Path, PathBuf};

use coder_precommit::git_operator::{
    auto_branch, capture_git, default_commit_message, git_config_idempotent, rebase_onto, run_git,
    Bundle,
};
use tokio::process::Command;

#[test]
fn auto_branch_format() {
    assert_eq!(auto_branch("brf_123"), "auto/brf_123");
}

#[test]
fn bundle_deserialize_minimal() {
    let json = r#"{
        "brief": {
            "id": "brf_42",
            "payload": {
                "target_repo": "yg/agentry",
                "base_branch": "develop",
                "pr_title": "title",
                "pr_body": "body"
            }
        }
    }"#;
    let bundle: Bundle = serde_json::from_str(json).expect("deserialize");
    assert_eq!(bundle.brief.id, "brf_42");
    assert_eq!(bundle.brief.payload.target_repo, "yg/agentry");
    assert_eq!(bundle.brief.payload.base_branch, "develop");
    assert_eq!(bundle.brief.payload.pr_title, "title");
    assert_eq!(bundle.brief.payload.pr_body, "body");
    assert_eq!(
        bundle.brief.payload.commit_message,
        default_commit_message()
    );
    assert!(bundle.brief.payload.forge_host.is_none());
}

async fn init_origin_and_work(root: &Path) -> (PathBuf, PathBuf) {
    let bare = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir(&work).expect("mkdir work");
    let bare_str = bare.to_str().expect("bare utf8");
    Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "--bare", bare_str])
        .current_dir(root)
        .output()
        .await
        .expect("git init --bare");
    Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init"])
        .current_dir(&work)
        .output()
        .await
        .expect("git init work");
    git_config_idempotent(&work).await.expect("git config");
    run_git(&work, &["remote", "add", "origin", bare_str])
        .await
        .expect("remote add");
    std::fs::write(work.join("file.txt"), "line1\n").expect("write file");
    run_git(&work, &["add", "."]).await.expect("git add");
    run_git(&work, &["commit", "-m", "init"])
        .await
        .expect("git commit");
    run_git(&work, &["push", "-u", "origin", "main"])
        .await
        .expect("git push");
    (bare, work)
}

#[tokio::test]
async fn rebase_onto_is_no_op_on_already_in_sync() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_bare, work) = init_origin_and_work(dir.path()).await;
    rebase_onto(&work, "main").await.expect("rebase clean");
    let status = capture_git(&work, &["status", "--porcelain"])
        .await
        .expect("status");
    assert!(
        status.trim().is_empty(),
        "worktree should be clean after no-op rebase: {status}"
    );
}

#[tokio::test]
async fn rebase_onto_aborts_on_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_bare, work) = init_origin_and_work(dir.path()).await;

    std::fs::write(work.join("file.txt"), "version-A\n").expect("write A");
    run_git(&work, &["commit", "-am", "version-A"])
        .await
        .expect("commit A");
    run_git(&work, &["push", "origin", "main"])
        .await
        .expect("push A");

    run_git(&work, &["checkout", "-b", "feature", "HEAD~1"])
        .await
        .expect("checkout feature");
    std::fs::write(work.join("file.txt"), "version-B\n").expect("write B");
    run_git(&work, &["commit", "-am", "version-B"])
        .await
        .expect("commit B");

    let result = rebase_onto(&work, "main").await;
    assert!(result.is_err(), "expected rebase to fail with conflict");

    let status = capture_git(&work, &["status", "--porcelain"])
        .await
        .expect("status");
    assert!(
        status.trim().is_empty(),
        "rebase --abort should leave worktree clean: {status}"
    );
}
