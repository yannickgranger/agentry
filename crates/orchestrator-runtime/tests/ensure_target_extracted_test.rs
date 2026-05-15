#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use orchestrator_runtime::intake_validation::{
    ensure_target_extracted, EnsureExtractedOutcome, EnsureExtractedRequest,
};
use tempfile::tempdir;

#[test]
fn cache_hit_when_marker_matches_sha() {
    let work_root = tempdir().unwrap();
    let slug = "yg_test"; // matches sanitize_target_repo_slug("yg/test")
    let db_dir = work_root.path().join("cfdb").join(slug);
    std::fs::create_dir_all(&db_dir).unwrap();
    std::fs::write(db_dir.join(format!("{slug}.head_sha")), "abc123\n").unwrap();
    // also touch an empty keyspace so cache_hit doesn't trip on missing data
    std::fs::write(db_dir.join(format!("{slug}.json")), "{}").unwrap();

    let req = EnsureExtractedRequest {
        target_repo: "yg/test".into(),
        head_sha: "abc123".into(),
        clone_url: "https://example.invalid/yg/test.git".into(),
        work_root: work_root.path().to_path_buf(),
    };

    match ensure_target_extracted(&req) {
        EnsureExtractedOutcome::CacheHit => {}
        other => panic!("expected CacheHit, got {other:?}"),
    }
}

#[test]
fn miss_when_marker_sha_differs() {
    let work_root = tempdir().unwrap();
    let slug = "yg_test";
    let db_dir = work_root.path().join("cfdb").join(slug);
    std::fs::create_dir_all(&db_dir).unwrap();
    std::fs::write(db_dir.join(format!("{slug}.head_sha")), "old_sha\n").unwrap();

    // No live network in tests; we can't actually run git clone. The expected
    // outcome here is Failed (clone or cfdb spawn fails). The test just
    // confirms that cache miss does NOT short-circuit to CacheHit.
    let req = EnsureExtractedRequest {
        target_repo: "yg/test".into(),
        head_sha: "new_sha".into(),
        clone_url: "https://localhost:1/will-not-resolve.git".into(),
        work_root: work_root.path().to_path_buf(),
    };

    match ensure_target_extracted(&req) {
        EnsureExtractedOutcome::CacheHit => panic!("must NOT cache-hit on sha mismatch"),
        EnsureExtractedOutcome::Extracted { .. } | EnsureExtractedOutcome::Failed { .. } => {}
    }
}

#[test]
fn cache_miss_returns_failed_when_clone_url_unreachable() {
    let work_root = tempdir().unwrap();
    let req = EnsureExtractedRequest {
        target_repo: "yg/never_existed".into(),
        head_sha: "deadbeef".into(),
        clone_url: "https://localhost:1/never-existed.git".into(),
        work_root: work_root.path().to_path_buf(),
    };
    match ensure_target_extracted(&req) {
        EnsureExtractedOutcome::Failed { reason } => {
            assert!(!reason.is_empty(), "Failed reason must be non-empty");
        }
        other => panic!("expected Failed for unreachable clone, got {other:?}"),
    }
}
