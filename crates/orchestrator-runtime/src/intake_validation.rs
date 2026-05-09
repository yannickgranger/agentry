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

use crate::anchor_resolver::{self, AnchorResolution, ResolverContext};
use orchestrator_types::contract::AssertionId;
use orchestrator_types::Brief;
use std::path::{Path, PathBuf};

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
/// `brief.payload.target_repo` + `workspace_root`, then run the existing
/// contract validator.
///
/// Briefs without a `target_repo` payload field fall back to the slug
/// `_unknown`. F1a does not enforce presence — the daemon caller in F1b is
/// responsible for pre-validating the payload.
pub fn validate_brief_contract_for_target(
    brief: &Brief,
    workspace_root: &Path,
) -> Vec<(AssertionId, String)> {
    let target = brief
        .payload
        .get("target_repo")
        .and_then(|v| v.as_str())
        .unwrap_or("_unknown");
    let ctx = ResolverContext::for_target_repo(target, workspace_root);
    validate_brief_contract(brief, &ctx)
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
