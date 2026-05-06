use quality_fast::{cargo_check_targets_with, derive_changed_crates, Check};

fn stub_runner() -> impl FnMut(&str, &str, &[&str]) -> Check {
    |name: &str, _prog: &str, _args: &[&str]| Check {
        name: name.to_string(),
        ok: true,
        stdout: String::new(),
        stderr: String::new(),
    }
}

#[test]
fn derive_changed_crates_groups_files_by_owning_crate() {
    let changed = vec![
        "crates/quality-fast/src/main.rs".to_string(),
        "crates/quality-fast/Cargo.toml".to_string(),
        "crates/orchestrator-runtime/src/lib.rs".to_string(),
        "specs/concepts/allowed_tools.md".to_string(),
        "Cargo.toml".to_string(),
    ];
    let crates = derive_changed_crates(&changed);
    assert_eq!(
        crates,
        vec![
            "orchestrator-runtime".to_string(),
            "quality-fast".to_string(),
        ]
    );
}

#[test]
fn derive_changed_crates_ignores_non_crate_paths() {
    let changed = vec![
        "specs/concepts/foo.md".to_string(),
        "scripts/arch-check.sh".to_string(),
        "Cargo.toml".to_string(),
    ];
    assert!(derive_changed_crates(&changed).is_empty());
}

#[test]
fn cargo_check_targets_emits_check_then_clippy_per_crate() {
    let crates = vec!["c1".to_string(), "c2".to_string()];
    let checks = cargo_check_targets_with(&crates, stub_runner());
    let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "cargo-check[c1]",
            "cargo-check[c2]",
            "cargo-clippy[c1]",
            "cargo-clippy[c2]",
        ]
    );
}

#[test]
fn cargo_check_targets_passes_expected_args() {
    let crates = vec!["c1".to_string()];
    let mut captured: Vec<(String, String, Vec<String>)> = Vec::new();
    let runner = |name: &str, prog: &str, args: &[&str]| {
        captured.push((
            name.to_string(),
            prog.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        ));
        Check {
            name: name.to_string(),
            ok: true,
            stdout: String::new(),
            stderr: String::new(),
        }
    };
    let _ = cargo_check_targets_with(&crates, runner);
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].0, "cargo-check[c1]");
    assert_eq!(captured[0].1, "cargo");
    assert_eq!(captured[0].2, vec!["check", "-p", "c1", "--all-targets"]);
    assert_eq!(captured[1].0, "cargo-clippy[c1]");
    assert_eq!(captured[1].1, "cargo");
    assert_eq!(
        captured[1].2,
        vec![
            "clippy",
            "-p",
            "c1",
            "--all-targets",
            "--",
            "-D",
            "warnings"
        ]
    );
}

#[test]
fn cargo_check_targets_skips_when_no_changed_crates() {
    let checks = cargo_check_targets_with(&[], stub_runner());
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].name, "cargo-check");
    assert!(checks[0].ok);
    assert_eq!(checks[0].stderr, "no changed Rust crates");
    assert!(checks[0].stdout.is_empty());
}
