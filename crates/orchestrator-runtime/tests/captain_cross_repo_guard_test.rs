//! Integration tests for the `captain new-brief` cross-repo --acceptance
//! guard: when --target-repo points outside yg/agentry, the agentry self-host
//! default acceptance command must not be silently emitted, because it
//! references workspace-local binaries (quality-fast, scripts/arch-check.sh)
//! that do not exist in the target repo. The guard lives in `build_brief`
//! inside the `captain` binary; these tests exercise it via the compiled
//! binary because `build_brief` is private to the bin crate.

use orchestrator_types::Brief;
use std::process::Command;

const DEFAULT_ACCEPTANCE: &str =
    "cargo run -p quality-fast --bin quality-mech --release --quiet && bash scripts/arch-check.sh";

fn captain_bin() -> &'static str {
    env!("CARGO_BIN_EXE_captain")
}

fn run_new_brief(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(captain_bin());
    cmd.arg("new-brief");
    cmd.args(args);
    cmd.output().expect("spawn captain new-brief")
}

#[test]
fn build_brief_errors_on_cross_repo_without_acceptance() {
    let out = run_new_brief(&[
        "--kind",
        "mechanical",
        "--target-repo",
        "yg/qbot-core",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "pr title",
        "--issue-title",
        "issue title",
    ]);
    assert!(
        !out.status.success(),
        "expected non-zero exit when cross-repo and --acceptance omitted, got status={:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("yg/qbot-core"),
        "error message must mention the target_repo, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("--acceptance is required"),
        "error message must contain `--acceptance is required`, got stderr: {stderr}"
    );
}

#[test]
fn build_brief_succeeds_on_cross_repo_with_explicit_acceptance() {
    let out = run_new_brief(&[
        "--kind",
        "mechanical",
        "--target-repo",
        "yg/qbot-core",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "pr title",
        "--issue-title",
        "issue title",
        "--acceptance",
        "cargo test",
    ]);
    assert!(
        out.status.success(),
        "expected success when --acceptance is provided for cross-repo, got status={:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let text = std::str::from_utf8(&out.stdout).expect("stdout is utf-8");
    let brief: Brief = serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("parse Brief from stdout failed: {e}\nstdout was:\n{text}"));
    assert_eq!(brief.payload["acceptance"], "cargo test");
}

#[test]
fn build_brief_uses_default_acceptance_on_agentry_self_host() {
    let out = run_new_brief(&[
        "--kind",
        "mechanical",
        "--target-repo",
        "yg/agentry",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "pr title",
        "--issue-title",
        "issue title",
    ]);
    assert!(
        out.status.success(),
        "expected success on agentry self-host with omitted --acceptance, got status={:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let text = std::str::from_utf8(&out.stdout).expect("stdout is utf-8");
    let brief: Brief = serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("parse Brief from stdout failed: {e}\nstdout was:\n{text}"));
    assert_eq!(brief.payload["acceptance"], DEFAULT_ACCEPTANCE);
}
