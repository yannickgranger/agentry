//! Per-brief host workspace — a scratch directory allocated at brief dispatch,
//! bind-mounted into each role that declares a `workspace_mount`, torn down
//! on `Shipped` (retained on failure for audit).
//!
//! Minimal implementation: `mkdir -p <root>/briefs/<brief_id>/` + `rm -rf` on
//! teardown. Coder roles do `git clone` into `/workspace` inside their
//! container. Bare-clone + `git worktree add` comes later — see agentry#10.
//!
//! The root defaults to `/var/mnt/workspaces/agentry-work/`. Override via the
//! `AGENTRY_WORKSPACE_ROOT` env var when running on another machine.

use crate::{Error, Result};
use orchestrator_types::BriefId;
use std::path::{Path, PathBuf};

/// Default host root — overridable via `AGENTRY_WORKSPACE_ROOT`.
const DEFAULT_ROOT: &str = "/var/mnt/workspaces/agentry-work";

/// Live handle to a brief's host workspace. Held by the daemon across the
/// brief's roles; bind-mounted into each role that declares it.
#[derive(Debug, Clone)]
pub struct BriefWorkspace {
    pub brief_id: BriefId,
    pub host_path: PathBuf,
}

impl BriefWorkspace {
    /// Root under which all per-brief workspaces live for this process.
    /// Reads `AGENTRY_WORKSPACE_ROOT` at call time; falls back to `DEFAULT_ROOT`.
    #[must_use]
    pub fn root() -> PathBuf {
        std::env::var("AGENTRY_WORKSPACE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_ROOT))
    }
}

/// Allocate a fresh workspace dir for this brief, under the default root.
/// Reads `AGENTRY_WORKSPACE_ROOT` from the environment (see `BriefWorkspace::root`).
pub async fn allocate(brief_id: &BriefId) -> Result<BriefWorkspace> {
    allocate_at(brief_id, &BriefWorkspace::root()).await
}

/// Allocate under an explicit root. Useful for tests that must not share the
/// process-wide env var. Idempotent: an existing path is not an error.
pub async fn allocate_at(brief_id: &BriefId, root: &Path) -> Result<BriefWorkspace> {
    let host_path = root.join("briefs").join(&brief_id.0);
    tokio::fs::create_dir_all(&host_path)
        .await
        .map_err(|e| Error::Config(format!("workspace allocate {}: {e}", host_path.display())))?;
    Ok(BriefWorkspace {
        brief_id: brief_id.clone(),
        host_path,
    })
}

/// Destroy a workspace. Safe on a nonexistent path (no-op).
///
/// Called by the daemon only on `VerdictKind::Shipped` — on any other verdict
/// the workspace is retained for audit until a future `orchestrator prune`.
pub async fn destroy(ws: &BriefWorkspace) -> Result<()> {
    if !ws.host_path.exists() {
        return Ok(());
    }
    remove_dir_all(&ws.host_path)
        .await
        .map_err(|e| Error::Config(format!("workspace destroy {}: {e}", ws.host_path.display())))
}

async fn remove_dir_all(path: &Path) -> std::io::Result<()> {
    // tokio::fs::remove_dir_all exists and handles non-empty dirs.
    tokio::fs::remove_dir_all(path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(id: &str) -> BriefId {
        BriefId(id.into())
    }

    #[tokio::test]
    async fn allocate_creates_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = allocate_at(&brief("brf_alloc_test"), tmp.path())
            .await
            .expect("alloc");
        assert!(ws.host_path.exists());
        assert!(ws.host_path.is_dir());
        assert!(ws.host_path.ends_with("briefs/brf_alloc_test"));
    }

    #[tokio::test]
    async fn destroy_removes_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = allocate_at(&brief("brf_destroy_test"), tmp.path())
            .await
            .expect("alloc");
        tokio::fs::write(ws.host_path.join("hello"), b"hi")
            .await
            .expect("write");
        destroy(&ws).await.expect("destroy");
        assert!(!ws.host_path.exists());
    }

    #[tokio::test]
    async fn destroy_nonexistent_is_noop() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = BriefWorkspace {
            brief_id: brief("brf_missing"),
            host_path: tmp.path().join("briefs/brf_missing_never_created"),
        };
        destroy(&ws).await.expect("destroy noop");
    }

    #[test]
    fn root_defaults_when_env_absent() {
        // This test doesn't mutate env (parallel-safe).
        let path = BriefWorkspace::root();
        // Either an env-provided value or the compile-time default.
        assert!(path.is_absolute(), "root should be absolute");
    }
}
