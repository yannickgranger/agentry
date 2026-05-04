//! End-to-end test: invoke the `ship` binary as a subprocess against a
//! tempdir Rust workspace and validate the JSON it prints.
//!
//! Marked `#[ignore]` because it shells out to `cargo run --bin ship`,
//! which inside `cargo test` recurses into the build system and is too
//! slow for CI's default test pass.

use std::process::Command;

#[test]
#[ignore = "spawns cargo build of the ship binary; run manually with --ignored"]
fn ship_emits_expected_json_shape() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path();

    // Make the dir a git repo with one commit so `git diff` doesn't blow up.
    run(ws, "git", &["init", "-q", "-b", "main"]);
    run(ws, "git", &["config", "user.email", "ship-e2e@test"]);
    run(ws, "git", &["config", "user.name", "ship-e2e"]);
    run(
        ws,
        "cargo",
        &["init", "--quiet", "--name", "ship-e2e-fixture"],
    );
    run(ws, "git", &["add", "-A"]);
    run(ws, "git", &["commit", "-q", "-m", "init"]);

    // Branch off main and add a commit so the diff is non-empty.
    run(ws, "git", &["checkout", "-q", "-b", "feature"]);
    std::fs::write(ws.join("src/extra.rs"), "// extra\n").expect("write");
    run(ws, "git", &["add", "-A"]);
    run(ws, "git", &["commit", "-q", "-m", "extra"]);

    // Find the workspace manifest of THIS repo so `cargo run --bin ship`
    // builds the binary under test.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");

    let out = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--manifest-path",
            &format!("{manifest}/Cargo.toml"),
            "--bin",
            "ship",
        ])
        .env("AGENTRY_BRIEF_ID", "test-1")
        .env("AGENTRY_BRIEF_KIND", "mechanical")
        .env("AGENTRY_BASE_BRANCH", "main")
        .current_dir(ws)
        .output()
        .expect("ship subprocess");

    assert!(
        out.status.success(),
        "ship exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The binary is hard-coded to inspect /workspace, so under the test
    // harness the validators will fail or no-op — that's fine. We only
    // assert structural properties of the JSON output.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().last().expect("at least one line on stdout");
    let v: serde_json::Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("ship stdout is not JSON: {e}\n{stdout}"));

    assert!(v["ok"].is_boolean(), "ok must be bool: {v}");
    assert_eq!(v["brief_id"], "test-1");
    assert_eq!(v["kind"], "mechanical");
    assert!(v["validators"].is_array(), "validators must be array: {v}");
    assert!(
        !v["validators"].as_array().expect("array").is_empty(),
        "validators must be non-empty: {v}"
    );
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
