//! Verifies that a malformed role JSON in seed/roles/ surfaces as Error::RoleLoadFailed
//! with the offending file path, instead of panicking the daemon at boot.

use orchestrator_runtime::role_dir_loader::read_and_parse_role;
use orchestrator_runtime::Error;

#[tokio::test]
async fn malformed_role_json_returns_role_load_failed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bogus.json");
    std::fs::write(&path, b"{ not valid json").expect("write bogus");

    let err = read_and_parse_role(&path)
        .await
        .expect_err("malformed JSON must propagate as Err, not panic");

    match err {
        Error::RoleLoadFailed { path: reported, .. } => {
            assert_eq!(
                reported.file_name().and_then(|s| s.to_str()),
                Some("bogus.json"),
                "RoleLoadFailed must name the offending file",
            );
        }
        other => panic!("expected RoleLoadFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_file_returns_role_load_failed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("does-not-exist.json");

    let err = read_and_parse_role(&path)
        .await
        .expect_err("missing file must propagate as Err, not panic");

    match err {
        Error::RoleLoadFailed { path: reported, .. } => {
            assert_eq!(
                reported.file_name().and_then(|s| s.to_str()),
                Some("does-not-exist.json"),
                "RoleLoadFailed must name the offending file",
            );
        }
        other => panic!("expected RoleLoadFailed, got {other:?}"),
    }
}
