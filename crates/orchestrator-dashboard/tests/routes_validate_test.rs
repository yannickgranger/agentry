//! Migrated from `src/routes/validate.rs`'s inline `#[cfg(test)]` block
//! (EPIC #256). The validators are crate-private (`pub(crate) mod
//! validate;`) and the migration brief forbids promoting them to `pub`
//! purely to satisfy tests. So this file exercises every validation
//! invariant — `brief_id` charset/length, `role` charset/length, and
//! `within_root` symlink defence — through the only public consumer
//! that actually calls them: the brief router (`routes::briefs`).
//!
//! `tests/integration_transcript_api.rs` already covers the basic
//! happy path and a handful of traversal cases via the same router.
//! This file deliberately complements it by drilling into the edge
//! cases the inline `#[cfg(test)]` block had: length caps on both
//! identifiers, the dot-in-id rejection, and a positive-case round
//! trip that proves valid identifiers reach the file-not-found branch
//! (404) rather than the validator's reject branch (400).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use orchestrator_dashboard::routes::briefs::{router, BriefsState};
use tower::ServiceExt;

const BRIEF_ID_MAX: usize = 256;
const ROLE_MAX: usize = 128;

fn req(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("build request")
}

#[tokio::test]
async fn brief_id_overlong_is_400() {
    let tmp = tempfile::tempdir().expect("tmp");
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let id: String = "a".repeat(BRIEF_ID_MAX + 1);
    let res = app
        .oneshot(req(&format!("/briefs/{id}/transcript")))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "brief id past length cap must be rejected"
    );
}

#[tokio::test]
async fn brief_id_with_dot_is_400() {
    let tmp = tempfile::tempdir().expect("tmp");
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req("/briefs/a.b/transcript"))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "dot in brief id must be rejected by charset gate"
    );
}

#[tokio::test]
async fn brief_id_valid_charset_passes_validator_and_404s() {
    let tmp = tempfile::tempdir().expect("tmp");
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    // No transcript file exists → handler must hit 404 (file-not-found),
    // proving the validator accepted the id.
    let res = app
        .oneshot(req("/briefs/AbC_123-xyz/transcript"))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "valid brief id must reach file-not-found, not 400"
    );
}

#[tokio::test]
async fn role_overlong_is_400() {
    let tmp = tempfile::tempdir().expect("tmp");
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let role: String = "a".repeat(ROLE_MAX + 1);
    let res = app
        .oneshot(req(&format!("/briefs/brf_x/transcript?role={role}")))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "role past length cap must be rejected"
    );
}

#[tokio::test]
async fn role_valid_passes_validator_and_404s() {
    let tmp = tempfile::tempdir().expect("tmp");
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    // Valid charset role, no file present → 404, proving the role
    // validator accepted the value.
    let res = app
        .oneshot(req("/briefs/brf_role_ok/transcript?role=reviewer-claude"))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "valid role must reach file-not-found, not 400"
    );
}

#[tokio::test]
async fn within_root_accepts_descendant() {
    let tmp = tempfile::tempdir().expect("tmp");
    let brief = "brf_descend";
    // Place the transcript inside the root — within_root must canonicalize
    // and accept it, leading to a 200.
    std::fs::write(tmp.path().join(format!("{brief}.jsonl")), b"x").expect("write transcript");
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript")))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "in-root descendant must reach 200 via within_root canonicalize"
    );
}

#[tokio::test]
async fn within_root_rejects_symlink_escape() {
    let tmp = tempfile::tempdir().expect("tmp");
    let brief = "brf_link";
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    let link = tmp.path().join(format!("{brief}.jsonl"));
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript")))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "symlink escape must be caught by within_root canonicalize-prefix check"
    );
}
