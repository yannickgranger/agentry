//! quality-mech — reviewer-grade scoped acceptance binary. Runs
//! `cargo clippy` on changed crates and `cargo test` on the
//! reverse-dependency closure of those crates. Falls back to all
//! workspace members when a workspace-root file changes (root
//! `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`). Intended for
//! use as the brief's `payload.acceptance` command on big Rust
//! workspaces where `cargo {clippy,test} --workspace` does not fit
//! the brief budget.

use anyhow::Result;
use quality_fast::{
    compute_rev_deps_closure, derive_changed_crates, run, workspace_member_names, Check,
};
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let base = parse_base(&args).unwrap_or_else(|| "develop".to_string());

    let changed = match changed_files(&base) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("quality-mech: {e}");
            std::process::exit(2);
        }
    };

    let mut changed_crates = derive_changed_crates(&changed);
    let workspace_root_touched = changed.iter().any(|p| is_workspace_root_path(p.as_str()));

    if workspace_root_touched {
        match workspace_member_names() {
            Ok(all) => changed_crates = all,
            Err(e) => {
                eprintln!("quality-mech: failed to enumerate workspace members: {e}");
                std::process::exit(2);
            }
        }
    }

    if changed_crates.is_empty() {
        println!("quality-mech: no Rust crates affected; skipping clippy + test");
        std::process::exit(0);
    }

    let closure = match compute_rev_deps_closure(&changed_crates) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("quality-mech: failed to compute rev-deps closure: {e}");
            std::process::exit(2);
        }
    };

    let mut checks: Vec<Check> = Vec::new();

    for c in &changed_crates {
        checks.push(run(
            &format!("clippy[{c}]"),
            "cargo",
            &["clippy", "-p", c, "--all-targets", "--", "-D", "warnings"],
        ));
    }

    for c in &closure {
        checks.push(run(&format!("test[{c}]"), "cargo", &["test", "-p", c]));
    }

    let ok = checks.iter().all(|c| c.ok);
    let payload = serde_json::json!({
        "ok": ok,
        "changed_crates": changed_crates,
        "closure": closure,
        "workspace_root_touched": workspace_root_touched,
        "checks": checks,
    });
    match serde_json::to_string(&payload) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("quality-mech: failed to serialize report: {e}");
            std::process::exit(1);
        }
    }
    std::process::exit(if ok { 0 } else { 1 });
}

fn parse_base(args: &[String]) -> Option<String> {
    let mut iter = args.iter().skip(1);
    while let Some(a) = iter.next() {
        if a == "--base" {
            return iter.next().cloned();
        }
        if let Some(rest) = a.strip_prefix("--base=") {
            return Some(rest.to_string());
        }
    }
    None
}

fn changed_files(base: &str) -> Result<Vec<String>> {
    let primary = format!("origin/{base}...HEAD");
    if let Some(v) = git_diff(&primary) {
        return Ok(v);
    }
    if let Some(v) = git_diff("HEAD~1..HEAD") {
        return Ok(v);
    }
    Err(anyhow::anyhow!(
        "quality-mech requires a known diff base; tried `origin/{base}...HEAD` and `HEAD~1..HEAD`, both failed"
    ))
}

fn git_diff(range: &str) -> Option<Vec<String>> {
    let out = Command::new("git")
        .args(["diff", "--name-only", range])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

fn is_workspace_root_path(p: &str) -> bool {
    matches!(p, "Cargo.toml" | "Cargo.lock" | "rust-toolchain.toml")
}
