//! quality-fast — small standalone binary that runs external CLI checks
//! against pre-paid indices for fast feedback. Scope is changed files
//! only (`git diff --name-only HEAD`); changed crates drive cargo fmt,
//! cargo check, and cargo clippy. Substrate validators handle
//! workspace-wide compile and test in the slow tier.

use anyhow::Result;
use quality_fast::{cargo_check_targets_with, derive_changed_crates, run, Check};
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let changed = changed_files().unwrap_or_default();
    let changed_crates = derive_changed_crates(&changed);

    let mut checks: Vec<Check> = Vec::new();

    for c in &changed_crates {
        checks.push(run(
            &format!("cargo-fmt[{c}]"),
            "cargo",
            &["fmt", "--check", "-p", c],
        ));
    }

    checks.extend(cargo_check_targets_with(&changed_crates, run));

    checks.extend(cfdb_checks(&changed));
    checks.extend(ra_query_checks(&changed_crates));
    checks.push(run("arch-check", "bash", &["scripts/arch-check.sh"]));

    let ok = checks.iter().all(|c| c.ok);
    let payload = serde_json::json!({ "ok": ok, "checks": checks });
    match serde_json::to_string(&payload) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("quality-fast: failed to serialize report: {e}");
            std::process::exit(1);
        }
    }
    std::process::exit(if ok { 0 } else { 1 });
}

fn changed_files() -> Result<Vec<String>> {
    let out = Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .output()?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn cfdb_checks(changed: &[String]) -> Vec<Check> {
    if !is_on_path("cfdb") {
        return vec![Check::skipped("cfdb", "cfdb binary not on PATH; skipping")];
    }
    let supports_files = match Command::new("cfdb").args(["query", "--help"]).output() {
        Ok(out) => {
            let blob = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            blob.contains("--files")
        }
        Err(_) => false,
    };
    if !supports_files {
        return vec![Check::skipped(
            "cfdb",
            "cfdb query lacks --files scoping; skipping until pre-paid scoped query lands",
        )];
    }

    let queries_dir = PathBuf::from(".cfdb/queries");
    let entries = match std::fs::read_dir(&queries_dir) {
        Ok(rd) => rd,
        Err(_) => {
            return vec![Check::skipped(
                "cfdb",
                ".cfdb/queries directory missing; skipping",
            )]
        }
    };

    let mut checks = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("cypher") {
            continue;
        }
        let name = format!(
            "cfdb[{}]",
            path.file_name().and_then(|s| s.to_str()).unwrap_or("")
        );
        let path_str = path.to_string_lossy().into_owned();
        let mut argv: Vec<String> = vec!["query".into(), path_str];
        if !changed.is_empty() {
            argv.push("--files".into());
            argv.push(changed.join(","));
        }
        let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        checks.push(run(&name, "cfdb", &argv_refs));
    }
    checks
}

fn ra_query_checks(changed_crates: &[String]) -> Vec<Check> {
    if !is_on_path("ra-query") {
        return vec![Check::skipped(
            "ra-query",
            "ra-query binary not on PATH; skipping",
        )];
    }
    let mut checks = Vec::new();
    for c in changed_crates {
        let crate_path = format!("crates/{c}");
        checks.push(run(
            &format!("ra-query-pub-surface[{c}]"),
            "ra-query",
            &["pub-surface", &crate_path],
        ));
    }
    checks
}

fn is_on_path(bin: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {bin}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
