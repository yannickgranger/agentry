use quality_fast::{rev_deps_closure_from_metadata, workspace_member_names_from_metadata};
use serde_json::json;

fn synth_metadata(packages: Vec<(&str, Vec<&str>)>) -> serde_json::Value {
    let ws_members: Vec<String> = packages
        .iter()
        .map(|(name, _)| format!("path+file:///fake/{name}#{name}@0.1.0"))
        .collect();
    let pkg_values: Vec<serde_json::Value> = packages
        .iter()
        .map(|(name, deps)| {
            let id = format!("path+file:///fake/{name}#{name}@0.1.0");
            json!({
                "id": id,
                "name": name,
                "dependencies": deps.iter().map(|d| json!({"name": d, "kind": null})).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "packages": pkg_values,
        "workspace_members": ws_members,
    })
}

#[test]
fn closure_empty_input_is_empty() {
    let md = synth_metadata(vec![("a", vec![]), ("b", vec!["a"])]);
    let out = rev_deps_closure_from_metadata(&md, &[]).expect("ok");
    assert!(out.is_empty());
}

#[test]
fn closure_single_leaf_returns_just_itself() {
    let md = synth_metadata(vec![("a", vec![]), ("b", vec!["a"])]);
    let out = rev_deps_closure_from_metadata(&md, &["b".to_string()]).expect("ok");
    assert_eq!(out, vec!["b".to_string()]);
}

#[test]
fn closure_dependency_change_includes_dependents() {
    let md = synth_metadata(vec![("a", vec![]), ("b", vec!["a"])]);
    let out = rev_deps_closure_from_metadata(&md, &["a".to_string()]).expect("ok");
    assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn closure_is_transitive() {
    let md = synth_metadata(vec![("c", vec![]), ("b", vec!["c"]), ("a", vec!["b"])]);
    let out = rev_deps_closure_from_metadata(&md, &["c".to_string()]).expect("ok");
    assert_eq!(out, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
}

#[test]
fn closure_ignores_non_workspace_deps() {
    let md = synth_metadata(vec![
        ("a", vec!["serde", "anyhow"]),
        ("b", vec!["a", "serde"]),
    ]);
    let out = rev_deps_closure_from_metadata(&md, &["a".to_string()]).expect("ok");
    assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn closure_unrelated_branch_excluded() {
    let md = synth_metadata(vec![
        ("a", vec![]),
        ("b", vec!["a"]),
        ("c", vec![]),
        ("d", vec!["c"]),
    ]);
    let out = rev_deps_closure_from_metadata(&md, &["a".to_string()]).expect("ok");
    assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn closure_seed_not_in_workspace_returns_just_seed() {
    // Seed not present in metadata: BFS visits seed, finds no rdeps, returns it.
    let md = synth_metadata(vec![("a", vec![])]);
    let out = rev_deps_closure_from_metadata(&md, &["nonexistent".to_string()]).expect("ok");
    assert_eq!(out, vec!["nonexistent".to_string()]);
}

#[test]
fn workspace_member_names_listed_sorted() {
    let md = synth_metadata(vec![
        ("zeta", vec![]),
        ("alpha", vec!["zeta"]),
        ("beta", vec![]),
    ]);
    let names = workspace_member_names_from_metadata(&md).expect("ok");
    assert_eq!(
        names,
        vec!["alpha".to_string(), "beta".to_string(), "zeta".to_string()]
    );
}

#[test]
fn closure_diamond_dependency() {
    // a -> b, a -> c, b -> d, c -> d. Changing d pulls a, b, c, d.
    let md = synth_metadata(vec![
        ("d", vec![]),
        ("b", vec!["d"]),
        ("c", vec!["d"]),
        ("a", vec!["b", "c"]),
    ]);
    let out = rev_deps_closure_from_metadata(&md, &["d".to_string()]).expect("ok");
    assert_eq!(
        out,
        vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string()
        ]
    );
}

// Integration test for the binary: builds a tiny cargo workspace in a
// tempdir, modifies a leaf crate, invokes the quality-mech binary, and
// asserts the JSON report shape. Skipped cleanly when cargo isn't on
// PATH in the test environment.

use std::path::{Path, PathBuf};
use std::process::Command;

fn is_cargo_available() -> bool {
    Command::new("cargo")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("qmech-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).expect("create tempdir");
        Self(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_file(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(&p, contents).expect("write file");
}

fn git(args: &[&str], cwd: &Path) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn quality_mech_binary_runs_against_synthetic_workspace() {
    if !is_cargo_available() || !is_git_available() {
        return;
    }
    let bin = match option_env!("CARGO_BIN_EXE_quality-mech") {
        Some(p) => p,
        None => return,
    };

    let dir = TempDir::new("ws");
    let root = dir.path();

    write_file(
        root,
        "Cargo.toml",
        r#"[workspace]
resolver = "2"
members = ["crates/member-a", "crates/member-b"]

[workspace.package]
version = "0.1.0"
edition = "2021"
"#,
    );
    write_file(
        root,
        "crates/member-a/Cargo.toml",
        r#"[package]
name = "member-a"
version = "0.1.0"
edition = "2021"

[dependencies]
member-b = { path = "../member-b" }
"#,
    );
    write_file(
        root,
        "crates/member-a/src/lib.rs",
        "pub fn a() -> u32 { member_b::b() }\n",
    );
    write_file(
        root,
        "crates/member-b/Cargo.toml",
        r#"[package]
name = "member-b"
version = "0.1.0"
edition = "2021"
"#,
    );
    write_file(
        root,
        "crates/member-b/src/lib.rs",
        "pub fn b() -> u32 { 1 }\n",
    );

    assert!(git(&["init", "-q"], root), "git init");
    assert!(git(&["add", "."], root), "git add");
    assert!(
        git(&["commit", "-q", "-m", "initial"], root),
        "initial commit"
    );

    write_file(
        root,
        "crates/member-b/src/lib.rs",
        "pub fn b() -> u32 { 2 }\n",
    );
    assert!(git(&["add", "."], root), "git add 2");
    assert!(
        git(&["commit", "-q", "-m", "tweak b"], root),
        "second commit"
    );

    let out = Command::new(bin)
        .current_dir(root)
        .output()
        .expect("spawn quality-mech");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "quality-mech exit={:?} stdout={stdout} stderr={stderr}",
        out.status.code()
    );

    let report: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse JSON failed: {e}; stdout={stdout}"));
    assert_eq!(report["ok"], serde_json::Value::Bool(true));
    assert_eq!(
        report["workspace_root_touched"],
        serde_json::Value::Bool(false)
    );
    let changed: Vec<String> = report["changed_crates"]
        .as_array()
        .expect("changed_crates array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    let closure: Vec<String> = report["closure"]
        .as_array()
        .expect("closure array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(changed, vec!["member-b".to_string()]);
    assert_eq!(
        closure,
        vec!["member-a".to_string(), "member-b".to_string()]
    );

    let check_names: Vec<String> = report["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .map(|c| c["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(check_names.contains(&"clippy[member-b]".to_string()));
    assert!(check_names.contains(&"test[member-a]".to_string()));
    assert!(check_names.contains(&"test[member-b]".to_string()));
}
