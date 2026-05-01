//! Migrated from `src/routes/validate.rs`'s inline `#[cfg(test)]` block
//! (EPIC #256). All assertions reach `validate` through the public
//! `orchestrator_dashboard::routes::validate` path.

use axum::http::StatusCode;
use orchestrator_dashboard::routes::validate;

const BRIEF_ID_MAX: usize = 256;

#[test]
fn brief_id_accepts_valid() {
    assert!(validate::brief_id("brf_01HZ").is_ok());
    assert!(validate::brief_id("a").is_ok());
    assert!(validate::brief_id("AbC_123-xyz").is_ok());
}

#[test]
fn brief_id_rejects_traversal_chars() {
    assert!(validate::brief_id("..").is_err());
    assert!(validate::brief_id("../etc/passwd").is_err());
    assert!(validate::brief_id("/etc/shadow").is_err());
    assert!(validate::brief_id("a/b").is_err());
    assert!(validate::brief_id("a.b").is_err());
    assert!(validate::brief_id("").is_err());
}

#[test]
fn brief_id_rejects_overlong() {
    let s: String = "a".repeat(BRIEF_ID_MAX + 1);
    assert!(validate::brief_id(&s).is_err());
}

#[test]
fn role_rejects_traversal() {
    assert!(validate::role("..").is_err());
    assert!(validate::role("../evil").is_err());
    assert!(validate::role("coder").is_ok());
    assert!(validate::role("reviewer-claude").is_ok());
}

#[tokio::test]
async fn within_root_accepts_descendant() {
    let tmp = tempfile::tempdir().expect("tmp");
    let child = tmp.path().join("child.jsonl");
    tokio::fs::write(&child, b"x").await.expect("write");
    let canon = validate::within_root(&child, tmp.path()).await.expect("ok");
    let root_canon = tokio::fs::canonicalize(tmp.path())
        .await
        .expect("canon root");
    assert!(canon.starts_with(&root_canon));
}

#[tokio::test]
async fn within_root_rejects_symlink_escape() {
    let tmp = tempfile::tempdir().expect("tmp");
    let outside = tempfile::NamedTempFile::new().expect("outside");
    let link = tmp.path().join("escape.jsonl");
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    let res = validate::within_root(&link, tmp.path()).await;
    assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
}
