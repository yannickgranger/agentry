//! Integration tests for the real validator impls.
//!
//! These run actual subprocesses against a tempdir-scaffolded crate. The
//! cargo-driven tests (`clippy_scoped_*`) are `#[ignore]` because they
//! compile a fresh crate and cost 30+ seconds — too slow for the default
//! `cargo test --workspace` budget. The fmt_check and arch_check tests
//! run by default per the brief's requirements.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use validators::impls::{ARCH_CHECK, CLIPPY_SCOPED, DEAD_PUB_CHECK, FMT_CHECK};
use validators::{BriefCtx, Severity, Validator};

/// Lay down a minimal, fmt-clean Rust crate at `dir`.
fn scaffold_crate(dir: &Path, crate_name: &str) {
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"

[dependencies]
"#
        ),
    )
    .expect("write Cargo.toml");
    fs::create_dir_all(dir.join("src")).expect("mkdir src");
    fs::write(
        dir.join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .expect("write main.rs");
}

fn ctx_for(dir: &Path, brief_id: &str, changed: Vec<PathBuf>) -> BriefCtx {
    BriefCtx {
        workspace_path: dir.to_path_buf(),
        brief_id: brief_id.into(),
        changed_files: changed,
    }
}

#[tokio::test]
async fn fmt_check_passes_on_clean_workspace() {
    let tmp = TempDir::new().expect("tempdir");
    scaffold_crate(tmp.path(), "clean_fixture");

    let ctx = ctx_for(tmp.path(), "brf_fmt_clean", vec![]);
    let report = FMT_CHECK.run(&ctx).await.expect("fmt_check ran");
    assert!(
        report.passed,
        "expected pass on fmt-clean fixture, got findings: {:?}",
        report.findings
    );
    assert!(report.findings.is_empty());
    assert_eq!(report.validator_name, "fmt_check");
}

#[tokio::test]
async fn fmt_check_fails_on_dirty_workspace() {
    let tmp = TempDir::new().expect("tempdir");
    scaffold_crate(tmp.path(), "dirty_fixture");
    // Mis-indent (tab + extra spaces) trips rustfmt's formatting check.
    fs::write(
        tmp.path().join("src/main.rs"),
        "fn main() {\n\t  println!(  \"hello\" )  ;\n   }\n",
    )
    .expect("write dirty main.rs");

    let ctx = ctx_for(tmp.path(), "brf_fmt_dirty", vec![]);
    let report = FMT_CHECK.run(&ctx).await.expect("fmt_check ran");
    assert!(!report.passed, "expected fail on dirty fixture");
    assert_eq!(report.findings.len(), 1, "one Blocker finding expected");
    assert_eq!(report.findings[0].severity, Severity::Blocker);
}

#[tokio::test]
async fn arch_check_passes_when_script_absent() {
    let tmp = TempDir::new().expect("tempdir");
    // Deliberately no scripts/arch-check.sh: the validator must no-op.
    let ctx = ctx_for(tmp.path(), "brf_arch_absent", vec![]);
    let report = ARCH_CHECK.run(&ctx).await.expect("arch_check ran");
    assert!(report.passed);
    assert!(report.findings.is_empty());
    assert_eq!(report.validator_name, "arch_check");
}

#[tokio::test]
async fn dead_pub_check_skips_when_binary_missing() {
    // The validator hard-codes /usr/local/bin/dead-pub-check. If it's not
    // present on the test host (the default for local dev outside the
    // bind-mounted role container), the validator must no-op.
    if Path::new("/usr/local/bin/dead-pub-check").is_file() {
        eprintln!(
            "skipping dead_pub_check_skips_when_binary_missing: binary is \
             present on this host; the binary-missing branch can't be \
             exercised here"
        );
        return;
    }
    let tmp = TempDir::new().expect("tempdir");
    let ctx = ctx_for(tmp.path(), "brf_dpc_missing", vec![]);
    let report = DEAD_PUB_CHECK.run(&ctx).await.expect("dead_pub_check ran");
    assert!(report.passed);
    assert!(report.findings.is_empty());
    assert_eq!(report.validator_name, "dead_pub_check");
}

/// Slow: invokes `cargo clippy` on a fresh tempdir crate. Compiles the
/// dependency graph and the local crate, ~30s+ on a cold cache. Marked
/// `#[ignore]` so `cargo test --workspace` stays under budget; opt in
/// with `cargo test -p validators --test impls -- --ignored`.
#[tokio::test]
#[ignore]
async fn clippy_scoped_falls_back_to_workspace_when_changed_files_empty() {
    let tmp = TempDir::new().expect("tempdir");
    scaffold_crate(tmp.path(), "scoped_fallback");

    let ctx = ctx_for(tmp.path(), "brf_clippy_fallback", vec![]);
    let report = CLIPPY_SCOPED.run(&ctx).await.expect("clippy_scoped ran");
    assert!(
        report.passed,
        "expected pass on clippy-clean fixture, got findings: {:?}",
        report.findings
    );
    assert_eq!(report.validator_name, "clippy_scoped");
}

/// Slow: also `#[ignore]` for the same compile-cost reason as above.
#[tokio::test]
#[ignore]
async fn clippy_scoped_finds_failing_crate() {
    let tmp = TempDir::new().expect("tempdir");
    let crate_name = "failing_fixture";
    scaffold_crate(tmp.path(), crate_name);
    // `#![deny(unused_variables)]` with an unused binding fails clippy.
    fs::write(
        tmp.path().join("src/main.rs"),
        "#![deny(unused_variables)]\nfn main() {\n    let x = 42;\n}\n",
    )
    .expect("write failing main.rs");

    let ctx = ctx_for(
        tmp.path(),
        "brf_clippy_fail",
        vec![PathBuf::from("src/main.rs")],
    );
    let report = CLIPPY_SCOPED.run(&ctx).await.expect("clippy_scoped ran");
    assert!(!report.passed, "expected fail on broken fixture");
    assert_eq!(report.findings.len(), 1);
    assert!(
        report.findings[0].message.contains(crate_name),
        "finding message should name the failing crate: {}",
        report.findings[0].message
    );
}
