//! Library half of `quality-fast`: pure planning/derivation helpers
//! exposed for unit testing. The binary in `src/main.rs` re-imports
//! these and wires them to the actual `cargo` / `cfdb` / `ra-query`
//! / `bash` runners. Side-effecting code stays in the binary so the
//! library remains test-friendly with no process spawning.

use anyhow::{anyhow, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::process::Command;

#[derive(Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Check {
    pub fn skipped(name: &str, reason: &str) -> Self {
        Self {
            name: name.to_string(),
            ok: true,
            stdout: String::new(),
            stderr: reason.to_string(),
        }
    }
}

/// Group `git diff --name-only HEAD` output into the set of owning
/// crate names under `crates/<name>/...`. Paths outside `crates/` are
/// ignored. Result is sorted and deduplicated.
pub fn derive_changed_crates(changed: &[String]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for path in changed {
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && parts[0] == "crates" {
            set.insert(parts[1].to_string());
        }
    }
    set.into_iter().collect()
}

/// Build the `cargo-check` / `cargo-clippy` check sequence for the
/// given changed crates. The sequence is `cargo-check[c]` for every
/// crate followed by `cargo-clippy[c]` for every crate. When
/// `changed_crates` is empty, returns a single
/// `Check::skipped("cargo-check", "no changed Rust crates")` so the
/// report shape stays informative.
///
/// `runner` is invoked with `(name, prog, args)` for each entry that
/// would actually run. Tests pass a stub that records calls without
/// spawning processes; the binary passes its real `run` helper.
pub fn cargo_check_targets_with<F>(changed_crates: &[String], mut runner: F) -> Vec<Check>
where
    F: FnMut(&str, &str, &[&str]) -> Check,
{
    if changed_crates.is_empty() {
        return vec![Check::skipped("cargo-check", "no changed Rust crates")];
    }
    let mut checks = Vec::new();
    for c in changed_crates {
        checks.push(runner(
            &format!("cargo-check[{c}]"),
            "cargo",
            &["check", "-p", c, "--all-targets"],
        ));
    }
    for c in changed_crates {
        checks.push(runner(
            &format!("cargo-clippy[{c}]"),
            "cargo",
            &["clippy", "-p", c, "--all-targets", "--", "-D", "warnings"],
        ));
    }
    checks
}

/// Spawn a child process and capture its result as a `Check`. Used by
/// both `quality-fast` and `quality-mech` to drive cargo / bash subprocesses.
pub fn run(name: &str, prog: &str, args: &[&str]) -> Check {
    match Command::new(prog).args(args).output() {
        Ok(out) => Check {
            name: name.to_string(),
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(e) => Check {
            name: name.to_string(),
            ok: false,
            stdout: String::new(),
            stderr: format!("failed to spawn {prog}: {e}"),
        },
    }
}

/// Walk `cargo metadata --format-version 1` to compute the
/// reverse-dependency closure of `changed_crates` over workspace
/// members. The closure contains every workspace package that is in
/// `changed_crates` OR depends transitively on any package in it.
///
/// Returns an empty vector when `changed_crates` is empty. Errors on
/// `cargo metadata` invocation failure or JSON parse failure.
pub fn compute_rev_deps_closure(changed_crates: &[String]) -> Result<Vec<String>> {
    if changed_crates.is_empty() {
        return Ok(Vec::new());
    }
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .output()
        .map_err(|e| anyhow!("failed to spawn cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow!("failed to parse cargo metadata JSON: {e}"))?;
    rev_deps_closure_from_metadata(&metadata, changed_crates)
}

/// Pure helper that operates on a parsed `cargo metadata` document.
/// Exposed for unit testing without spawning cargo.
pub fn rev_deps_closure_from_metadata(
    metadata: &serde_json::Value,
    changed_crates: &[String],
) -> Result<Vec<String>> {
    if changed_crates.is_empty() {
        return Ok(Vec::new());
    }

    let ws_member_ids: HashSet<String> = metadata
        .get("workspace_members")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("cargo metadata: workspace_members missing or not an array"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    let packages = metadata
        .get("packages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("cargo metadata: packages missing or not an array"))?;

    let mut ws_names: HashSet<String> = HashSet::new();
    for pkg in packages {
        let id = pkg.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if ws_member_ids.contains(id) {
            if let Some(name) = pkg.get("name").and_then(|v| v.as_str()) {
                ws_names.insert(name.to_string());
            }
        }
    }

    let mut forward_deps: HashMap<String, HashSet<String>> = HashMap::new();
    for pkg in packages {
        let id = pkg.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !ws_member_ids.contains(id) {
            continue;
        }
        let name = match pkg.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let entry = forward_deps.entry(name).or_default();
        if let Some(deps) = pkg.get("dependencies").and_then(|v| v.as_array()) {
            for dep in deps {
                if let Some(dep_name) = dep.get("name").and_then(|v| v.as_str()) {
                    if ws_names.contains(dep_name) {
                        entry.insert(dep_name.to_string());
                    }
                }
            }
        }
    }

    let mut reverse_deps: HashMap<String, HashSet<String>> = HashMap::new();
    for (pkg, deps) in &forward_deps {
        for d in deps {
            reverse_deps
                .entry(d.clone())
                .or_default()
                .insert(pkg.clone());
        }
    }

    let mut closure: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = changed_crates.to_vec();
    while let Some(node) = queue.pop() {
        if !closure.insert(node.clone()) {
            continue;
        }
        if let Some(rdeps) = reverse_deps.get(&node) {
            for r in rdeps {
                if !closure.contains(r) {
                    queue.push(r.clone());
                }
            }
        }
    }

    Ok(closure.into_iter().collect())
}

/// List every workspace member's package name via `cargo metadata`.
/// Used by `quality-mech` when a workspace-root file changes (e.g.
/// `Cargo.toml` at the repo root, `Cargo.lock`, `rust-toolchain.toml`)
/// — root-level changes affect every crate, so scoping would miss
/// regressions and we fall back to the full member list.
pub fn workspace_member_names() -> Result<Vec<String>> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| anyhow!("failed to spawn cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow!("failed to parse cargo metadata JSON: {e}"))?;
    workspace_member_names_from_metadata(&metadata)
}

/// Pure helper for `workspace_member_names`. Exposed for tests.
pub fn workspace_member_names_from_metadata(metadata: &serde_json::Value) -> Result<Vec<String>> {
    let ws_member_ids: HashSet<String> = metadata
        .get("workspace_members")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("cargo metadata: workspace_members missing or not an array"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    let packages = metadata
        .get("packages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("cargo metadata: packages missing or not an array"))?;

    let mut names: BTreeSet<String> = BTreeSet::new();
    for pkg in packages {
        let id = pkg.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if ws_member_ids.contains(id) {
            if let Some(name) = pkg.get("name").and_then(|v| v.as_str()) {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names.into_iter().collect())
}
