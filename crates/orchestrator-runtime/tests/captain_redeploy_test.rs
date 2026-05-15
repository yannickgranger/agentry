#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn captain_redeploy_help_lists_target_and_dry_run() {
    let out = Command::cargo_bin("captain")
        .expect("captain binary built")
        .args(["redeploy", "--help"])
        .output()
        .expect("spawn captain redeploy --help");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(predicates::str::contains("--target").eval(stdout.as_ref()));
    assert!(predicates::str::contains("--dry-run").eval(stdout.as_ref()));
    assert!(predicates::str::contains("daemon").eval(stdout.as_ref()));
    assert!(predicates::str::contains("orchestrator-cli").eval(stdout.as_ref()));
    assert!(predicates::str::contains("captain-cli").eval(stdout.as_ref()));
}

#[test]
fn captain_redeploy_dry_run_lists_targets_without_building() {
    let out = Command::cargo_bin("captain")
        .expect("captain binary built")
        .args(["redeploy", "--target", "daemon", "--dry-run"])
        .output()
        .expect("spawn captain redeploy");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(predicates::str::contains("target=daemon").eval(stderr.as_ref()));
    assert!(predicates::str::contains("dry-run").eval(stderr.as_ref()));
}

#[test]
fn captain_redeploy_rejects_unknown_target() {
    let out = Command::cargo_bin("captain")
        .expect("captain binary built")
        .args(["redeploy", "--target", "made-up", "--dry-run"])
        .output()
        .expect("spawn captain redeploy");
    assert!(
        !out.status.success(),
        "expected non-zero for unknown target"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(predicates::str::contains("unknown redeploy target").eval(stderr.as_ref()));
}

#[test]
fn captain_redeploy_all_default_lists_three_targets_in_dry_run() {
    let out = Command::cargo_bin("captain")
        .expect("captain binary built")
        .args(["redeploy", "--dry-run"])
        .output()
        .expect("spawn captain redeploy");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(predicates::str::contains("target=daemon").eval(stderr.as_ref()));
    assert!(predicates::str::contains("target=orchestrator-cli").eval(stderr.as_ref()));
    assert!(predicates::str::contains("target=captain-cli").eval(stderr.as_ref()));
}
