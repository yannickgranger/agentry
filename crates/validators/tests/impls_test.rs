use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use validators::impls::CLIPPY_SCOPED;
use validators::{BriefCtx, Validator};

#[tokio::test]
async fn clippy_scoped_passes_when_only_workspace_manifest_found() {
    // ClippyScoped walks each changed file's ancestors looking for a
    // Cargo.toml that declares a `[package]` section. A workspace-only
    // manifest must NOT match — the validator should fall through with
    // crates.is_empty() and return pass without invoking cargo.
    let tmp = TempDir::new().expect("tempdir");
    fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[workspace]
members = ["a", "b"]
"#,
    )
    .expect("write workspace Cargo.toml");
    fs::create_dir_all(tmp.path().join("a/src")).expect("mkdir a/src");
    fs::write(tmp.path().join("a/src/lib.rs"), "// no package manifest\n").expect("write src");

    let ctx = BriefCtx {
        workspace_path: tmp.path().to_path_buf(),
        brief_id: "brf_workspace_only".into(),
        changed_files: vec![PathBuf::from("a/src/lib.rs")],
    };
    let report = CLIPPY_SCOPED.run(&ctx).await.expect("clippy_scoped ran");
    assert!(
        report.passed,
        "expected pass on workspace-only manifest, got findings: {:?}",
        report.findings
    );
    assert!(report.findings.is_empty());
    assert_eq!(report.validator_name, "clippy_scoped");
}
