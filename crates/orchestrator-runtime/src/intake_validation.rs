//! Brief-level intake validator built on top of [`anchor_resolver`].
//!
//! [`validate_brief_contract`] iterates a brief's contract assertions and
//! resolves each anchor against the local agentry workspace's cfdb keyspace
//! and `specs/concepts/` directory, returning a list of `(AssertionId,
//! reason)` pairs for any anchor that does not resolve.
//!
//! Brief-kind WARNs (`requires_contract && contract.is_none()`) are
//! intentionally OUT OF SCOPE here — that observation lives in `daemon.rs`
//! and remains log-only per B3.
//!
//! [`anchor_resolver`]: crate::anchor_resolver

use crate::anchor_resolver::{self, sanitize_target_repo_slug, AnchorResolution, ResolverContext};
use orchestrator_types::contract::AssertionId;
use orchestrator_types::Brief;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Typed intake-rejection error.
///
/// Brief 1b: the daemon admits a brief only when its `target_repo` parses
/// and the parsed owner is in `cfg.forge.allowed_owners`. The two pre-mint
/// gates raise these variants — there is no permissive fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntakeError {
    /// `payload.target_repo` is absent, non-string, or fails
    /// [`TargetRepo::from_str`] charset/length validation.
    ///
    /// [`TargetRepo::from_str`]: orchestrator_types::TargetRepo::from_str
    MissingTargetRepo,
    /// `target_repo.owner()` is not in `cfg.forge.allowed_owners`.
    OwnerNotAllowed { owner: String },
}

impl std::fmt::Display for IntakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTargetRepo => f.write_str(
                "brief intake rejected: payload.target_repo missing or fails strict validation",
            ),
            Self::OwnerNotAllowed { owner } => write!(
                f,
                "brief intake rejected: target_repo owner `{owner}` is not in [forge] allowed_owners",
            ),
        }
    }
}

impl std::error::Error for IntakeError {}

/// Resolve every assertion anchor in `brief.contract` against `ctx`.
///
/// Returns `(AssertionId, reason)` pairs for anchors that did not resolve.
/// Empty vec means all anchors resolved, the brief carries no contract, or
/// the contract has no assertions.
pub fn validate_brief_contract(brief: &Brief, ctx: &ResolverContext) -> Vec<(AssertionId, String)> {
    let mut failures: Vec<(AssertionId, String)> = Vec::new();
    if let Some(contract) = brief.contract.as_ref() {
        for assertion in &contract.assertions {
            match anchor_resolver::resolve_assertion(&assertion.anchor, ctx) {
                AnchorResolution::Resolved => {}
                AnchorResolution::NotFound { reason } => {
                    failures.push((assertion.id.clone(), reason));
                }
            }
        }
    }
    failures
}

/// Brief-aware entry point: derive a per-target [`ResolverContext`] from
/// `brief.target_repo()` + `workspace_root`, then run the contract validator.
///
/// Returns [`IntakeError::MissingTargetRepo`] when the brief lacks a
/// parseable `payload.target_repo` — there is no `_unknown` keyspace
/// fallback. The daemon caller treats this as a hard reject.
pub fn validate_brief_contract_for_target(
    brief: &Brief,
    workspace_root: &Path,
) -> Result<Vec<(AssertionId, String)>, IntakeError> {
    let target_repo = brief.target_repo().ok_or(IntakeError::MissingTargetRepo)?;
    let ctx = ResolverContext::for_target_repo(&target_repo.to_string(), workspace_root);
    Ok(validate_brief_contract(brief, &ctx))
}

/// Inputs to [`ensure_target_extracted`].
#[derive(Debug, Clone)]
pub struct EnsureExtractedRequest {
    pub target_repo: String,
    pub head_sha: String,
    pub clone_url: String,
    pub work_root: PathBuf,
}

/// Outcome of [`ensure_target_extracted`].
#[derive(Debug)]
pub enum EnsureExtractedOutcome {
    CacheHit,
    Extracted { items: usize },
    Failed { reason: String },
}

/// Populate the per-target_repo cfdb keyspace and specs cache under `work_root`.
///
/// Idempotent — keyed by `(slug + head_sha)` via a marker file. The function
/// is synchronous; the daemon (F1d) wraps it in `tokio::task::spawn_blocking`.
///
/// Layout under `work_root` after a successful extraction:
///   - `work_root/cfdb/<slug>/<slug>.json` — cfdb keyspace
///   - `work_root/cfdb/<slug>/<slug>.head_sha` — cache marker
///   - `work_root/specs/<slug>/` — copy of `specs/concepts/` (skipped if absent)
///
/// V1 limitation: shallow-clones the default branch HEAD rather than the
/// requested `head_sha`. F1c.tight (future) will fetch + checkout the exact
/// sha.
pub fn ensure_target_extracted(req: &EnsureExtractedRequest) -> EnsureExtractedOutcome {
    let slug = sanitize_target_repo_slug(&req.target_repo);
    let db_path = req.work_root.join("cfdb").join(&slug);
    let marker_path = db_path.join(format!("{slug}.head_sha"));

    if let Ok(existing) = std::fs::read_to_string(&marker_path) {
        if existing.trim_end_matches('\n') == req.head_sha {
            return EnsureExtractedOutcome::CacheHit;
        }
    }

    let clone_dir = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(e) => {
            return EnsureExtractedOutcome::Failed {
                reason: format!("tempdir creation failed: {e}"),
            };
        }
    };

    let clone_status = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            &req.clone_url,
            &clone_dir.path().display().to_string(),
        ])
        .output();
    match clone_status {
        Err(e) => {
            return EnsureExtractedOutcome::Failed {
                reason: format!("git clone spawn failed: {e}"),
            };
        }
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return EnsureExtractedOutcome::Failed {
                reason: format!("git clone failed: {}", stderr.trim()),
            };
        }
        Ok(_) => {}
    }

    if let Err(e) = std::fs::create_dir_all(&db_path) {
        return EnsureExtractedOutcome::Failed {
            reason: format!("cfdb db dir create failed at {}: {e}", db_path.display()),
        };
    }

    let extract_status = Command::new("cfdb")
        .args([
            "extract",
            "--workspace",
            &clone_dir.path().display().to_string(),
            "--db",
            &db_path.display().to_string(),
            "--keyspace",
            &slug,
        ])
        .output();
    match extract_status {
        Err(e) => {
            return EnsureExtractedOutcome::Failed {
                reason: format!("cfdb spawn failed: {e}"),
            };
        }
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return EnsureExtractedOutcome::Failed {
                reason: format!("cfdb extract failed: {}", stderr.trim()),
            };
        }
        Ok(_) => {}
    }

    let specs_src = clone_dir.path().join("specs/concepts");
    if specs_src.is_dir() {
        let specs_dst = req.work_root.join("specs").join(&slug);
        if let Err(e) = copy_dir_recursive(&specs_src, &specs_dst) {
            return EnsureExtractedOutcome::Failed {
                reason: format!(
                    "specs copy failed from {} to {}: {e}",
                    specs_src.display(),
                    specs_dst.display()
                ),
            };
        }
    }

    if let Err(e) = std::fs::write(&marker_path, &req.head_sha) {
        return EnsureExtractedOutcome::Failed {
            reason: format!("marker write failed at {}: {e}", marker_path.display()),
        };
    }

    let items = count_keyspace_items(&db_path.join(format!("{slug}.json")));
    EnsureExtractedOutcome::Extracted { items }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn count_keyspace_items(json_path: &Path) -> usize {
    let body = match std::fs::read_to_string(json_path) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    if let Some(nodes) = value.get("nodes").and_then(|n| n.as_array()) {
        return nodes.len();
    }
    if let Some(items) = value.get("items").and_then(|n| n.as_array()) {
        return items.len();
    }
    0
}

impl ResolverContext {
    /// Build a [`ResolverContext`] from environment variables.
    ///
    /// - `AGENTRY_CFDB_DB` — cfdb database path; defaults to
    ///   `/tmp/agentry-cfdb-db-local`.
    /// - `AGENTRY_CFDB_KEYSPACE` — cfdb keyspace; defaults to `agentry`.
    /// - `AGENTRY_SPECS_DIR` — `specs/concepts/` root; defaults to
    ///   `specs/concepts`.
    #[must_use]
    pub fn from_env() -> Self {
        let cfdb_db = std::env::var("AGENTRY_CFDB_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/agentry-cfdb-db-local"));
        let cfdb_keyspace =
            std::env::var("AGENTRY_CFDB_KEYSPACE").unwrap_or_else(|_| "agentry".to_string());
        let specs_dir = std::env::var("AGENTRY_SPECS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("specs/concepts"));
        Self {
            cfdb_db,
            cfdb_keyspace,
            specs_dir,
        }
    }
}
