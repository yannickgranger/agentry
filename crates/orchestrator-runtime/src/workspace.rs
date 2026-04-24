//! Per-brief host workspace — a scratch directory allocated at brief dispatch,
//! bind-mounted into each role that declares a `workspace_mount`, torn down
//! on `Shipped` (retained on failure for audit).
//!
//! When the brief names a `(repo_url, base_branch)`, the workspace is a
//! `git worktree` off a shared bare clone at `<root>/.clones/<org>/<repo>/`.
//! The first brief against a given repo creates the bare clone via
//! `git clone --bare`; subsequent briefs `git fetch --prune` it and add a
//! fresh worktree. When no repo is named (probe roles: echo, naughty, etc.),
//! the workspace falls back to an empty scratch dir — preserving legacy
//! semantics.
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

/// Derive `<org>/<repo>` from a repo URL: take the trailing two non-empty
/// path components and strip a `.git` suffix from the final one. Examples:
/// `https://forge.example/org/name.git` → `("org", "name")`,
/// `https://forge.example/yg/agentry`   → `("yg", "agentry")`.
fn derive_org_repo(repo_url: &str) -> Result<(String, String)> {
    // Strip query/fragment if present.
    let trimmed = repo_url
        .split(['?', '#'])
        .next()
        .unwrap_or(repo_url)
        .trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return Err(Error::Config(format!(
            "cannot derive org/repo from repo_url {repo_url}"
        )));
    }
    let org = parts[parts.len() - 2].to_string();
    let mut repo = parts[parts.len() - 1].to_string();
    if let Some(stripped) = repo.strip_suffix(".git") {
        repo = stripped.to_string();
    }
    if org.is_empty() || repo.is_empty() {
        return Err(Error::Config(format!(
            "cannot derive org/repo from repo_url {repo_url}"
        )));
    }
    Ok((org, repo))
}

/// Ensure a bare clone exists for `repo_url` at `<root>/.clones/<org>/<repo>/`.
/// If the bare clone is missing, `git clone --bare` creates it. If it exists,
/// `git fetch --prune` refreshes it. Returns the bare-clone path.
///
/// Derives `<org>/<repo>` from the URL's trailing two path components,
/// stripping a trailing `.git` if present. Example: a `repo_url` of
/// `https://.../yg/agentry.git` yields `<root>/.clones/yg/agentry/`.
pub async fn ensure_bare_clone(repo_url: &str, root: &Path) -> Result<PathBuf> {
    let (org, repo) = derive_org_repo(repo_url)?;
    let bare = root.join(".clones").join(&org).join(&repo);

    // Bare-clone marker: `HEAD` exists in any populated bare repo.
    let already_cloned = tokio::fs::metadata(bare.join("HEAD")).await.is_ok();

    if already_cloned {
        let out = tokio::process::Command::new("git")
            .arg("-c")
            .arg("http.sslVerify=false")
            .arg("-C")
            .arg(&bare)
            .arg("fetch")
            .arg("--prune")
            .output()
            .await
            .map_err(|e| Error::Config(format!("git fetch: {e}")))?;
        if !out.status.success() {
            return Err(Error::Config(format!(
                "git fetch failed for {}: {}",
                bare.display(),
                String::from_utf8_lossy(&out.stderr)
            )));
        }
    } else {
        if let Some(parent) = bare.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                Error::Config(format!(
                    "create bare-clone parent {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let out = tokio::process::Command::new("git")
            .arg("-c")
            .arg("http.sslVerify=false")
            .arg("clone")
            .arg("--bare")
            .arg(repo_url)
            .arg(&bare)
            .output()
            .await
            .map_err(|e| Error::Config(format!("git clone --bare: {e}")))?;
        if !out.status.success() {
            return Err(Error::Config(format!(
                "git clone --bare failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        // `git clone --bare` in modern git does NOT add a fetch refspec.
        // Without it, subsequent `git fetch --prune` updates only FETCH_HEAD —
        // local refs/heads/* stay frozen, and every brief's
        // `git worktree add -b auto/X <path> develop` forks from stale develop.
        // Set the standard refspec explicitly so fetches keep refs fresh.
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&bare)
            .arg("config")
            .arg("--add")
            .arg("remote.origin.fetch")
            .arg("+refs/heads/*:refs/heads/*")
            .output()
            .await
            .map_err(|e| Error::Config(format!("git config remote.origin.fetch: {e}")))?;
        if !out.status.success() {
            return Err(Error::Config(format!(
                "git config --add remote.origin.fetch failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
    }

    Ok(bare)
}

/// Allocate a fresh workspace for this brief under the default root.
/// When `repo` is `Some((url, base))`, the workspace is a git worktree
/// off a shared bare clone, checked out on branch `auto/<brief_id>`.
/// When `repo` is `None`, the legacy empty-scratch-dir path is used.
pub async fn allocate(brief_id: &BriefId, repo: Option<(&str, &str)>) -> Result<BriefWorkspace> {
    allocate_at(brief_id, repo, &BriefWorkspace::root()).await
}

/// Allocate under an explicit root. Useful for tests that must not share the
/// process-wide env var.
pub async fn allocate_at(
    brief_id: &BriefId,
    repo: Option<(&str, &str)>,
    root: &Path,
) -> Result<BriefWorkspace> {
    let host_path = root.join("briefs").join(&brief_id.0);
    if let Some((repo_url, base_branch)) = repo {
        let bare = ensure_bare_clone(repo_url, root).await?;
        // If the worktree dir already exists (e.g. resumed brief), remove it
        // first so the worktree add doesn't conflict.
        if host_path.exists() {
            tokio::fs::remove_dir_all(&host_path).await.map_err(|e| {
                Error::Config(format!("workspace pre-clean {}: {e}", host_path.display()))
            })?;
        }
        if let Some(parent) = host_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                Error::Config(format!("workspace parent {}: {e}", parent.display()))
            })?;
        }
        let branch = format!("auto/{}", brief_id.0);
        let out = tokio::process::Command::new("git")
            .arg("-c")
            .arg("http.sslVerify=false")
            .arg("-C")
            .arg(&bare)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(&branch)
            .arg(&host_path)
            .arg(base_branch)
            .output()
            .await
            .map_err(|e| Error::Config(format!("git worktree add: {e}")))?;
        if !out.status.success() {
            return Err(Error::Config(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
    } else {
        // Legacy path: empty scratch dir. Preserves probe roles that don't
        // name a repo.
        tokio::fs::create_dir_all(&host_path).await.map_err(|e| {
            Error::Config(format!("workspace allocate {}: {e}", host_path.display()))
        })?;
    }
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
    // Detect if this is a worktree (has a `.git` file, not dir). If so,
    // run `git worktree remove --force` so the bare-clone's admin dir
    // forgets this worktree. Falls back to plain rm -rf for legacy briefs.
    let dotgit = ws.host_path.join(".git");
    let is_worktree = tokio::fs::metadata(&dotgit)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false);
    if is_worktree {
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&ws.host_path)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(".")
            .output()
            .await;
        // Even on `worktree remove` success, the dir itself is gone now.
        // On failure, fall through to rm -rf.
    }
    if ws.host_path.exists() {
        tokio::fs::remove_dir_all(&ws.host_path)
            .await
            .map_err(|e| {
                Error::Config(format!("workspace destroy {}: {e}", ws.host_path.display()))
            })?;
    }
    Ok(())
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
        let ws = allocate_at(&brief("brf_alloc_test"), None, tmp.path())
            .await
            .expect("alloc");
        assert!(ws.host_path.exists());
        assert!(ws.host_path.is_dir());
        assert!(ws.host_path.ends_with("briefs/brf_alloc_test"));
    }

    #[tokio::test]
    async fn destroy_removes_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = allocate_at(&brief("brf_destroy_test"), None, tmp.path())
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

    #[test]
    fn derive_org_repo_strips_dot_git() {
        let (o, r) = derive_org_repo("https://forge.example/org/name.git").expect("derive");
        assert_eq!(o, "org");
        assert_eq!(r, "name");
    }

    #[test]
    fn derive_org_repo_without_dot_git() {
        let (o, r) = derive_org_repo("https://forge.example/yg/agentry").expect("derive");
        assert_eq!(o, "yg");
        assert_eq!(r, "agentry");
    }

    #[test]
    fn derive_org_repo_rejects_short_url() {
        assert!(derive_org_repo("foo").is_err());
        assert!(derive_org_repo("/").is_err());
    }

    async fn run_git(cwd: &Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .await
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Set up an empty bare upstream repo with one commit on `main` so that
    /// `git worktree add <path> main` has something to check out. Returns the
    /// upstream URL (file://...) usable as `repo_url`.
    async fn setup_upstream(dir: &Path) -> String {
        let upstream = dir.join("upstream.git");
        tokio::fs::create_dir_all(&upstream)
            .await
            .expect("mk upstream dir");
        // Initialize a non-bare seed repo, make one commit on `main`, then
        // clone --bare from it. The bare clone is what `ensure_bare_clone`
        // will fetch from, simulating a real forge upstream.
        let seed = dir.join("seed");
        tokio::fs::create_dir_all(&seed).await.expect("mk seed dir");
        run_git(&seed, &["init", "-q", "-b", "main"]).await;
        run_git(&seed, &["config", "user.email", "test@example.com"]).await;
        run_git(&seed, &["config", "user.name", "test"]).await;
        tokio::fs::write(seed.join("README"), b"hello\n")
            .await
            .expect("write README");
        run_git(&seed, &["add", "README"]).await;
        run_git(&seed, &["commit", "-q", "-m", "initial"]).await;
        // Now make `upstream.git` a bare clone of the seed.
        let out = tokio::process::Command::new("git")
            .args(["clone", "--bare", "-q"])
            .arg(&seed)
            .arg(&upstream)
            .output()
            .await
            .expect("git clone --bare upstream");
        assert!(
            out.status.success(),
            "git clone --bare upstream failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        format!("file://{}", upstream.display())
    }

    #[tokio::test]
    async fn ensure_bare_clone_creates_on_first_call_fetches_on_second() {
        let tmp = tempfile::tempdir().expect("tmp");
        let url = setup_upstream(tmp.path()).await;
        let bare1 = ensure_bare_clone(&url, tmp.path()).await.expect("first");
        assert!(bare1.join("HEAD").exists(), "bare clone HEAD must exist");
        // Second call should be a no-op fetch — same path.
        let bare2 = ensure_bare_clone(&url, tmp.path()).await.expect("second");
        assert_eq!(bare1, bare2);
        assert!(bare2.join("HEAD").exists());
    }

    #[tokio::test]
    async fn allocate_with_repo_creates_worktree_on_branch() {
        let tmp = tempfile::tempdir().expect("tmp");
        let url = setup_upstream(tmp.path()).await;
        let bid = brief("brf_wt_test");
        let ws = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
            .await
            .expect("alloc worktree");
        let dotgit = ws.host_path.join(".git");
        let meta = tokio::fs::metadata(&dotgit).await.expect("dotgit meta");
        assert!(meta.is_file(), ".git in a worktree is a file, not dir");
        // Confirm the checkout is on `auto/<brief_id>`.
        let out = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&ws.host_path)
            .output()
            .await
            .expect("rev-parse");
        assert!(out.status.success(), "rev-parse failed");
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(branch, format!("auto/{}", bid.0));
    }

    #[tokio::test]
    async fn destroy_worktree_leaves_bare_clone_intact() {
        let tmp = tempfile::tempdir().expect("tmp");
        let url = setup_upstream(tmp.path()).await;
        let bid = brief("brf_destroy_wt");
        let ws = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
            .await
            .expect("alloc");
        tokio::fs::write(ws.host_path.join("scratch"), b"x")
            .await
            .expect("write scratch");
        // The bare path is derived from the URL (last two components, minus
        // .git). Locate it via the same derivation the runtime uses.
        let (org, repo) = derive_org_repo(&url).expect("derive");
        let bare_dir = tmp.path().join(".clones").join(&org).join(&repo);
        assert!(
            bare_dir.join("HEAD").exists(),
            "bare clone HEAD missing pre-destroy at {}",
            bare_dir.display()
        );
        destroy(&ws).await.expect("destroy");
        assert!(!ws.host_path.exists(), "worktree dir survived destroy");
        assert!(
            bare_dir.join("HEAD").exists(),
            "destroy nuked the bare clone (must survive)"
        );
    }

    #[tokio::test]
    async fn ensure_bare_clone_sets_fetch_refspec() {
        let seed = tempfile::tempdir().expect("seed tmpdir");
        // Minimal upstream: a bare repo we can clone from via file:// URL.
        let seed_repo = seed.path().join("upstream.git");
        tokio::process::Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(&seed_repo)
            .output()
            .await
            .expect("git init --bare upstream");

        let root = tempfile::tempdir().expect("root tmpdir");
        let url = format!("file://{}", seed_repo.display());
        let bare = ensure_bare_clone(&url, root.path())
            .await
            .expect("ensure_bare_clone");

        // Read the refspec via `git config --get-all`. Must contain the
        // standard `+refs/heads/*:refs/heads/*` entry, otherwise `git fetch`
        // will not refresh local refs.
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&bare)
            .arg("config")
            .arg("--get-all")
            .arg("remote.origin.fetch")
            .output()
            .await
            .expect("git config --get-all");
        assert!(
            out.status.success(),
            "git config --get-all failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let refspecs = String::from_utf8(out.stdout).expect("utf8");
        assert!(
            refspecs
                .lines()
                .any(|l| l.trim() == "+refs/heads/*:refs/heads/*"),
            "expected +refs/heads/*:refs/heads/* in remote.origin.fetch, got: {refspecs:?}"
        );
    }
}
