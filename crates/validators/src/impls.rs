//! Real Validator implementations ported from the reviewer-mechanical bash
//! script in `orchestrator-runtime/src/seed.rs`.
//!
//! Each validator drives a single subprocess (cargo / git / bash /
//! `dead-pub-check`) and translates the result into a [`ValidatorReport`]:
//! pass on exit 0, otherwise a Blocker [`Finding`] with a 2 KiB tail of the
//! combined stderr+stdout output. Brief 4 wires `registry_for(brief.kind)`
//! to the ship tool — for now nothing reads these impls.

use async_trait::async_trait;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::debug;

use crate::{BriefCtx, Finding, Severity, Validator, ValidatorReport};

/// Reviewer-mechanical's combined-tail cap. Matches the 2000-char limit
/// used by `REVIEWER_MECHANICAL_AGENTRY_SCRIPT`.
const TAIL_BYTES: usize = 2048;

/// Per-brief target dir to avoid clobbering the coder's `target/`. The ship
/// tool may share this dir across validators in the same brief, but each
/// brief gets its own root.
fn target_dir(brief_id: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/agentry-validators/{brief_id}/target"))
}

/// Last `n` bytes of `s` as a `String`, snapped to a UTF-8 char boundary.
fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut start = s.len() - n;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// `tail(stderr) + "\n---stdout---\n" + tail(stdout)`, each segment capped.
fn combined_output(stderr: &[u8], stdout: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stdout = String::from_utf8_lossy(stdout);
    format!(
        "{}\n---stdout---\n{}",
        tail(&stderr, TAIL_BYTES),
        tail(&stdout, TAIL_BYTES)
    )
}

/// `cargo fmt --check` in the workspace.
pub struct FmtCheck;
pub static FMT_CHECK: FmtCheck = FmtCheck;

#[async_trait]
impl Validator for FmtCheck {
    fn name(&self) -> &'static str {
        "fmt_check"
    }
    async fn run(&self, ctx: &BriefCtx) -> anyhow::Result<ValidatorReport> {
        let out = Command::new("cargo")
            .args(["fmt", "--check"])
            .current_dir(&ctx.workspace_path)
            .env("CARGO_TARGET_DIR", target_dir(&ctx.brief_id))
            .output()
            .await?;
        if out.status.success() {
            return Ok(ValidatorReport::pass(self.name()));
        }
        Ok(ValidatorReport::fail(
            self.name(),
            vec![Finding {
                file: None,
                line: None,
                severity: Severity::Blocker,
                message: combined_output(&out.stderr, &out.stdout),
            }],
        ))
    }
}

/// `cargo clippy --workspace --all-targets -- -D warnings`.
pub struct ClippyWorkspace;
pub static CLIPPY_WORKSPACE: ClippyWorkspace = ClippyWorkspace;

#[async_trait]
impl Validator for ClippyWorkspace {
    fn name(&self) -> &'static str {
        "clippy_workspace"
    }
    async fn run(&self, ctx: &BriefCtx) -> anyhow::Result<ValidatorReport> {
        let out = Command::new("cargo")
            .args([
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ])
            .current_dir(&ctx.workspace_path)
            .env("CARGO_TARGET_DIR", target_dir(&ctx.brief_id))
            .output()
            .await?;
        if out.status.success() {
            return Ok(ValidatorReport::pass(self.name()));
        }
        Ok(ValidatorReport::fail(
            self.name(),
            vec![Finding {
                file: None,
                line: None,
                severity: Severity::Blocker,
                message: combined_output(&out.stderr, &out.stdout),
            }],
        ))
    }
}

/// `cargo clippy -p <crate>` for every crate touched by `ctx.changed_files`.
///
/// Walks each changed path up the directory tree until it finds a
/// `Cargo.toml` containing `[package]` — that file's `name = "..."` line
/// names the crate. When `changed_files` is empty (e.g. the ship tool
/// hasn't populated it yet), this validator falls back to
/// [`ClippyWorkspace`] behaviour so a brief without diff context still
/// gets a real check.
pub struct ClippyScoped;
pub static CLIPPY_SCOPED: ClippyScoped = ClippyScoped;

#[async_trait]
impl Validator for ClippyScoped {
    fn name(&self) -> &'static str {
        "clippy_scoped"
    }
    async fn run(&self, ctx: &BriefCtx) -> anyhow::Result<ValidatorReport> {
        if ctx.changed_files.is_empty() {
            // Mirror ClippyWorkspace's behaviour but report under our own name.
            let out = Command::new("cargo")
                .args([
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ])
                .current_dir(&ctx.workspace_path)
                .env("CARGO_TARGET_DIR", target_dir(&ctx.brief_id))
                .output()
                .await?;
            if out.status.success() {
                return Ok(ValidatorReport::pass(self.name()));
            }
            return Ok(ValidatorReport::fail(
                self.name(),
                vec![Finding {
                    file: None,
                    line: None,
                    severity: Severity::Blocker,
                    message: combined_output(&out.stderr, &out.stdout),
                }],
            ));
        }

        let mut crates: BTreeSet<String> = BTreeSet::new();
        for rel in &ctx.changed_files {
            let abs = ctx.workspace_path.join(rel);
            if let Some(name) = enclosing_crate_name(&abs).await? {
                crates.insert(name);
            }
        }

        if crates.is_empty() {
            // Changed files exist but none are inside a crate (e.g. only docs/
            // files). Nothing to lint.
            return Ok(ValidatorReport::pass(self.name()));
        }

        let mut findings = Vec::new();
        for crate_name in &crates {
            let out = Command::new("cargo")
                .args([
                    "clippy",
                    "-p",
                    crate_name,
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ])
                .current_dir(&ctx.workspace_path)
                .env("CARGO_TARGET_DIR", target_dir(&ctx.brief_id))
                .output()
                .await?;
            if !out.status.success() {
                let body = combined_output(&out.stderr, &out.stdout);
                findings.push(Finding {
                    file: None,
                    line: None,
                    severity: Severity::Blocker,
                    message: format!("crate `{crate_name}` failed clippy:\n{body}"),
                });
            }
        }

        if findings.is_empty() {
            Ok(ValidatorReport::pass(self.name()))
        } else {
            Ok(ValidatorReport::fail(self.name(), findings))
        }
    }
}

/// Walk `start`'s ancestors, return the `name` field of the first
/// `Cargo.toml` containing a `[package]` table. Returns `None` if no such
/// manifest exists between `start` and the filesystem root.
async fn enclosing_crate_name(start: &Path) -> anyhow::Result<Option<String>> {
    let mut cur: Option<&Path> = if start.is_dir() {
        Some(start)
    } else {
        start.parent()
    };
    while let Some(dir) = cur {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let body = tokio::fs::read_to_string(&manifest).await?;
            if has_package_section(&body) {
                if let Some(name) = parse_package_name(&body) {
                    return Ok(Some(name));
                }
                // [package] but no parseable name — bail out rather than
                // climbing past it into a virtual workspace manifest.
                return Ok(None);
            }
            // Virtual workspace (or other non-package manifest): keep climbing.
        }
        cur = dir.parent();
    }
    Ok(None)
}

fn has_package_section(toml: &str) -> bool {
    toml.lines()
        .map(str::trim)
        .any(|l| l == "[package]" || l.starts_with("[package]"))
}

fn parse_package_name(toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("name") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim();
        // Strip surrounding quotes if present.
        let name = rest
            .trim_start_matches('"')
            .trim_end_matches('"')
            .trim_start_matches('\'')
            .trim_end_matches('\'');
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// `cargo test --workspace`.
pub struct TestWorkspace;
pub static TEST_WORKSPACE: TestWorkspace = TestWorkspace;

#[async_trait]
impl Validator for TestWorkspace {
    fn name(&self) -> &'static str {
        "test_workspace"
    }
    async fn run(&self, ctx: &BriefCtx) -> anyhow::Result<ValidatorReport> {
        let out = Command::new("cargo")
            .args(["test", "--workspace"])
            .current_dir(&ctx.workspace_path)
            .env("CARGO_TARGET_DIR", target_dir(&ctx.brief_id))
            .output()
            .await?;
        if out.status.success() {
            return Ok(ValidatorReport::pass(self.name()));
        }
        Ok(ValidatorReport::fail(
            self.name(),
            vec![Finding {
                file: None,
                line: None,
                severity: Severity::Blocker,
                message: combined_output(&out.stderr, &out.stdout),
            }],
        ))
    }
}

/// `bash scripts/arch-check.sh` — agentry's spec/cfdb gate.
///
/// Tolerates the script not existing in non-agentry repos: when
/// `scripts/arch-check.sh` is absent the validator is a no-op and returns
/// [`ValidatorReport::pass`] with no findings. The ship tool is reused
/// across projects, only some of which use the arch-check pipeline.
pub struct ArchCheck;
pub static ARCH_CHECK: ArchCheck = ArchCheck;

#[async_trait]
impl Validator for ArchCheck {
    fn name(&self) -> &'static str {
        "arch_check"
    }
    async fn run(&self, ctx: &BriefCtx) -> anyhow::Result<ValidatorReport> {
        let script = ctx.workspace_path.join("scripts/arch-check.sh");
        if !script.is_file() {
            return Ok(ValidatorReport::pass(self.name()));
        }
        let out = Command::new("bash")
            .arg("scripts/arch-check.sh")
            .current_dir(&ctx.workspace_path)
            .output()
            .await?;
        if out.status.success() {
            return Ok(ValidatorReport::pass(self.name()));
        }
        Ok(ValidatorReport::fail(
            self.name(),
            vec![Finding {
                file: None,
                line: None,
                severity: Severity::Blocker,
                message: combined_output(&out.stderr, &out.stdout),
            }],
        ))
    }
}

/// Path to the bind-mounted `dead-pub-check` binary on the role host.
const DEAD_PUB_CHECK_BIN: &str = "/usr/local/bin/dead-pub-check";

/// Invokes the bind-mounted `dead-pub-check` binary with
/// `{diff, workspace_root}` JSON on stdin (matching the protocol in
/// `crates/coder-precommit/src/bin/dead_pub_check.rs`). Falls through to
/// pass when the binary is not present, mirroring the
/// `dead_pub_check_unavailable` warn-skip handling in the existing
/// reviewer-mechanical script.
pub struct DeadPubCheck;
pub static DEAD_PUB_CHECK: DeadPubCheck = DeadPubCheck;

#[async_trait]
impl Validator for DeadPubCheck {
    fn name(&self) -> &'static str {
        "dead_pub_check"
    }
    async fn run(&self, ctx: &BriefCtx) -> anyhow::Result<ValidatorReport> {
        if !Path::new(DEAD_PUB_CHECK_BIN).is_file() {
            debug!("dead_pub_check_unavailable: {DEAD_PUB_CHECK_BIN} not present; skipping gate");
            return Ok(ValidatorReport::pass(self.name()));
        }

        // Diff vs HEAD captures the coder's uncommitted changes — matches the
        // reviewer-mechanical seed which uses `git diff --cached` at
        // pre-commit time. Brief 4 may extend BriefCtx with base_branch.
        let diff_out = Command::new("git")
            .args(["diff", "-U0"])
            .current_dir(&ctx.workspace_path)
            .output()
            .await?;
        let diff = if diff_out.status.success() {
            String::from_utf8_lossy(&diff_out.stdout).to_string()
        } else {
            String::new()
        };

        let workspace_root = ctx.workspace_path.to_string_lossy().to_string();
        let payload = serde_json::json!({
            "diff": diff,
            "workspace_root": workspace_root,
        })
        .to_string();

        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let mut child = Command::new(DEAD_PUB_CHECK_BIN)
            .current_dir(&ctx.workspace_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload.as_bytes()).await?;
            stdin.shutdown().await?;
        }
        let out = child.wait_with_output().await?;
        if out.status.success() {
            return Ok(ValidatorReport::pass(self.name()));
        }
        Ok(ValidatorReport::fail(
            self.name(),
            vec![Finding {
                file: None,
                line: None,
                severity: Severity::Blocker,
                message: combined_output(&out.stderr, &out.stdout),
            }],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_short_strings_intact() {
        assert_eq!(tail("hello", 100), "hello");
    }

    #[test]
    fn tail_truncates_to_byte_window() {
        let s: String = "a".repeat(100);
        assert_eq!(tail(&s, 10).len(), 10);
    }

    #[test]
    fn tail_snaps_to_utf8_boundary() {
        // 'é' is 2 bytes (0xC3 0xA9). Asking for the last 5 bytes of "aééé"
        // (1 + 6 = 7 bytes) would land mid-codepoint at byte 2 — verify we
        // advance past the boundary instead of panicking on slice.
        let s = "aééé";
        let t = tail(s, 5);
        assert!(s.ends_with(&t));
    }

    #[test]
    fn parse_package_name_handles_quotes_and_whitespace() {
        let toml = r#"
[package]
name = "my-crate"
version = "0.1.0"
"#;
        assert_eq!(parse_package_name(toml).as_deref(), Some("my-crate"));
    }

    #[test]
    fn parse_package_name_returns_none_for_workspace_only() {
        let toml = r#"
[workspace]
members = ["a", "b"]
"#;
        assert!(parse_package_name(toml).is_none());
        assert!(!has_package_section(toml));
    }

    #[test]
    fn target_dir_is_per_brief() {
        let a = target_dir("brf_one");
        let b = target_dir("brf_two");
        assert_ne!(a, b);
        assert!(a.starts_with("/tmp/agentry-validators"));
    }
}
