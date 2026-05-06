//! Library half of `quality-fast`: pure planning/derivation helpers
//! exposed for unit testing. The binary in `src/main.rs` re-imports
//! these and wires them to the actual `cargo` / `cfdb` / `ra-query`
//! / `bash` runners. Side-effecting code stays in the binary so the
//! library remains test-friendly with no process spawning.

use serde::Serialize;
use std::collections::BTreeSet;

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
