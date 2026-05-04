use std::path::{Path, PathBuf};

use coder_precommit::git_operator::{
    auto_branch, capture_git, default_commit_message, emit_event_to, git_config_idempotent,
    rebase_onto, run_git, Bundle, DoneGuard,
};
use tokio::process::Command;

#[test]
fn auto_branch_format() {
    assert_eq!(auto_branch("brf_123"), "auto/brf_123");
}

#[test]
fn done_guard_emits_on_drop_when_not_emitted() {
    let mut buf = Vec::<u8>::new();
    let mut g = DoneGuard {
        emitted: false,
        brief_id: "brf_test".into(),
    };
    g.emit_drop_event_to(&mut buf)
        .expect("emit_drop_event_to write");
    g.emitted = true;
    drop(g);

    let line = String::from_utf8(buf).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(line.trim()).expect("parse json line");
    assert_eq!(v["type"], "done");
    assert_eq!(v["verdict"], "failed");
    assert_eq!(v["reason"]["brief"], "brf_test");
    assert_eq!(v["reason"]["unexpected_exit"], true);
}

#[test]
fn done_guard_silent_when_emitted() {
    let mut buf = Vec::<u8>::new();
    let g = DoneGuard {
        emitted: true,
        brief_id: "brf_test".into(),
    };
    g.emit_drop_event_to(&mut buf)
        .expect("emit_drop_event_to write");
    drop(g);
    assert!(
        buf.is_empty(),
        "guard with emitted=true must not write any extra event"
    );
}

#[test]
fn emit_event_injects_at_when_absent() {
    let mut buf = Vec::<u8>::new();
    emit_event_to(
        &mut buf,
        &serde_json::json!({
            "type": "progress",
            "message": "no at field here",
        }),
    )
    .expect("emit_event_to write");
    let line = String::from_utf8(buf).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(line.trim()).expect("parse json line");
    let at = v
        .get("at")
        .and_then(|v| v.as_str())
        .expect("emit_event must inject an `at` field when the payload lacks one");
    chrono::DateTime::parse_from_rfc3339(at)
        .expect("injected `at` value must be a valid RFC3339 timestamp");
}

#[test]
fn emit_event_preserves_existing_at() {
    const PRESET: &str = "2026-04-29T00:00:00+00:00";
    let mut buf = Vec::<u8>::new();
    emit_event_to(
        &mut buf,
        &serde_json::json!({
            "type": "progress",
            "at": PRESET,
            "message": "has its own at",
        }),
    )
    .expect("emit_event_to write");
    let line = String::from_utf8(buf).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(line.trim()).expect("parse json line");
    assert_eq!(
        v.get("at").and_then(|v| v.as_str()),
        Some(PRESET),
        "emit_event must NOT overwrite an existing `at` value"
    );
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
