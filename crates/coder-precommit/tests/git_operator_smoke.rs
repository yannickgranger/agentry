//! Smoke tests for the `git-operator` binary.
//!
//! Each test invokes the binary as a subprocess (built via `cargo run --bin
//! git-operator`) with `GIT_OPERATOR_WORKSPACE` pointing to a tempdir, so the
//! tests are hermetic and never touch the host's `/workspace`.
//!
//! Network-dependent paths (the PR-open call to gitea) are out of scope —
//! marked `#[ignore]` and not exercised by `cargo test`.

use std::process::{Command, Stdio};

const MIN_BUNDLE: &str = r#"{
    "brief": {
        "id": "brf_test_smoke",
        "payload": {
            "target_repo": "yg/agentry",
            "base_branch": "develop",
            "pr_title": "test",
            "pr_body": "test"
        }
    }
}"#;

#[test]
fn binary_emits_failed_done_when_workspace_missing_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_git_operator(tmp.path(), MIN_BUNDLE);
    assert!(
        !out.status.success(),
        "git-operator must exit non-zero when /workspace lacks .git"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("\"done\"") && stdout.contains("\"verdict\":\"failed\""),
        "stdout must contain a done failed event. stdout={stdout}\nstderr={stderr}"
    );
}

#[test]
fn binary_emits_failed_done_when_no_changes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path();

    run(ws, "git", &["init", "-q", "-b", "main"]);
    run(ws, "git", &["config", "user.email", "smoke@test"]);
    run(ws, "git", &["config", "user.name", "smoke"]);
    std::fs::write(ws.join("README.md"), "init\n").expect("write README");
    run(ws, "git", &["add", "-A"]);
    run(ws, "git", &["commit", "-q", "-m", "init"]);

    let out = run_git_operator(ws, MIN_BUNDLE);
    assert!(
        !out.status.success(),
        "git-operator must exit non-zero when there are no staged changes"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("no changes to commit"),
        "stdout must include the 'no changes to commit' diagnostic. stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("\"done\"") && stdout.contains("\"verdict\":\"failed\""),
        "stdout must contain a done failed event. stdout={stdout}\nstderr={stderr}"
    );
}

#[test]
fn binary_emits_failed_done_when_bundle_json_is_malformed() {
    // Reviewer-mandated regression test for the #160 silent-exit class:
    // even when the JSON parse fails BEFORE any state is set up, the
    // DoneGuard must still emit a terminal `done failed` event so the
    // orchestrator never observes a silent exit.
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_git_operator(tmp.path(), "this is not json {{{");
    assert!(
        !out.status.success(),
        "git-operator must exit non-zero on malformed bundle JSON"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("\"done\"") && stdout.contains("\"verdict\":\"failed\""),
        "stdout must contain a done failed event even when JSON parse fails. stdout={stdout}\nstderr={stderr}"
    );
}

#[test]
#[ignore = "requires a live gitea (or a mock) to exercise the PR-open path"]
fn binary_opens_pr_against_live_gitea() {
    // Placeholder: an integration harness with a gitea container would
    // exercise the full happy path. Out of scope for unit smoke tests.
}

fn run_git_operator(workspace: &std::path::Path, bundle: &str) -> std::process::Output {
    // CARGO_BIN_EXE_<name> is a path to the built binary that cargo sets for
    // integration tests — no nested `cargo run` build, unlike ship_e2e.
    let bin = env!("CARGO_BIN_EXE_git-operator");
    let mut child = Command::new(bin)
        .env("GITEA_TOKEN", "fake-token-for-tests")
        .env("GIT_OPERATOR_WORKSPACE", workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git-operator");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(bundle.as_bytes())
            .expect("write bundle");
    }
    child.wait_with_output().expect("wait git-operator")
}

fn run(cwd: &std::path::Path, prog: &str, args: &[&str]) {
    let out = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("spawn {prog}: {e}"));
    assert!(
        out.status.success(),
        "{prog} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
