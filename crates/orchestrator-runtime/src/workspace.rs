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
//! The root defaults to the value of the `XDG_DATA_HOME` env var joined with
//! `/agentry/work`, falling back to `HOME` joined with `/.local/share/agentry/work`
//! when `XDG_DATA_HOME` is unset. Override via the `AGENTRY_WORKSPACE_ROOT` env var.

use crate::{Error, Result};
use orchestrator_types::BriefId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::Mutex as TokioMutex;

/// Default host root — overridable via `AGENTRY_WORKSPACE_ROOT`.
fn default_root() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("agentry").join("work");
    }
    let home = std::env::var("HOME")
        .expect("either XDG_DATA_HOME or HOME must be set to derive the default workspace root");
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("agentry")
        .join("work")
}

/// Per-bare-clone async lock registry.
///
/// Concurrent `allocate` calls against the SAME bare clone race on
/// `git fetch --prune` and `git worktree add ... <base_branch>`: a fetch
/// can leave `refs/heads/<base>` transiently unborn while another worktree-add
/// resolves it, producing a worktree with unborn HEAD whose first commit is
/// orphaned (no parent). PRs from such worktrees are rejected by the forge
/// with "fatal: refusing to merge unrelated histories" — observed in A7v3
/// (PR #61) and A7v4 (PR #68).
///
/// The lock serializes the (fetch + worktree add) window per bare-clone path.
/// Allocations against DIFFERENT repos still run concurrently because each
/// has its own lock.
fn bare_clone_locks() -> &'static StdMutex<HashMap<PathBuf, Arc<TokioMutex<()>>>> {
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Arc<TokioMutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn lock_for_bare(bare: &Path) -> Arc<TokioMutex<()>> {
    let mut guard = bare_clone_locks().lock().unwrap_or_else(|poison| {
        tracing::warn!(lock = "bare_clone_locks", "recovering from poisoned mutex");
        poison.into_inner()
    });
    guard
        .entry(bare.to_path_buf())
        .or_insert_with(|| Arc::new(TokioMutex::new(())))
        .clone()
}

/// Live handle to a brief's host workspace. Held by the daemon across the
/// brief's roles; bind-mounted into each role that declares it.
#[derive(Debug, Clone)]
pub struct BriefWorkspace {
    pub brief_id: BriefId,
    pub host_path: PathBuf,
}

impl BriefWorkspace {
    /// Root under which all per-brief workspaces live for this process.
    /// Reads `AGENTRY_WORKSPACE_ROOT` at call time; falls back to `default_root()`.
    #[must_use]
    pub fn root() -> PathBuf {
        std::env::var("AGENTRY_WORKSPACE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_root())
    }
}

/// Whether the daemon should remove a brief's workspace on termination, or
/// preserve it on disk for forensics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationDisposition {
    TearDown,
    Preserve,
}

/// Map a verdict string (the same form persisted to `agentry:verdicts`) onto
/// the host-side teardown disposition for the brief's workspace.
///
/// Default is `Preserve`: any verdict not recognized as cleanly shipped (or
/// review-blocked, where the diff already lives in the forge as a PR) keeps
/// the workspace on disk so an operator can inspect the failure with
/// `agentry-workspace path <brief_id>`.
#[must_use]
pub fn disposition_for(verdict_str: &str) -> TerminationDisposition {
    match verdict_str {
        "shipped" => TerminationDisposition::TearDown,
        v if v.starts_with("review-blocked") => TerminationDisposition::TearDown,
        _ => TerminationDisposition::Preserve,
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
        // CRITICAL: do NOT pass `--prune`. The fetch refspec set on
        // `git clone --bare` is `+refs/heads/*:refs/heads/*` (mirror).
        // With `--prune`, git deletes any local `refs/heads/*` not present
        // on upstream — including the per-brief `auto/<brief_id>` branches
        // that prior allocations created via `git worktree add -b`. That
        // leaves their worktrees with unborn HEAD → coder produces an
        // orphan commit → forge merge fails with "refusing to merge
        // unrelated histories" (observed A7v3 PR #61, A7v4 PR #68).
        //
        // Without `--prune`, locally-created auto/* refs survive across
        // fetches. Stale upstream-deleted branches accumulate harmlessly
        // (they take space but don't affect correctness).
        let out = tokio::process::Command::new("git")
            .arg("-c")
            .arg("http.sslVerify=false")
            .arg("-C")
            .arg(&bare)
            .arg("fetch")
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

/// `git clone --local --no-hardlinks <bare> <host_path>` with `http.sslVerify=false`.
/// Gives the brief its own independent .git database (no shared object files,
/// no worktree linkage).
async fn git_local_clone(bare: &Path, host_path: &Path) -> Result<()> {
    let clone = tokio::process::Command::new("git")
        .arg("-c")
        .arg("http.sslVerify=false")
        .arg("clone")
        .arg("--local")
        .arg("--no-hardlinks")
        .arg(bare)
        .arg(host_path)
        .output()
        .await
        .map_err(|e| Error::Config(format!("git clone --local: {e}")))?;
    if !clone.status.success() {
        return Err(Error::Config(format!(
            "git clone --local failed: {}",
            String::from_utf8_lossy(&clone.stderr)
        )));
    }
    Ok(())
}

/// `git -C <host_path> remote set-url origin <repo_url>`. Repoints origin from
/// the local bare to the actual forge URL so push goes to the forge, not back
/// to the host bare.
async fn git_set_origin_url(host_path: &Path, repo_url: &str) -> Result<()> {
    let set_url = tokio::process::Command::new("git")
        .arg("-C")
        .arg(host_path)
        .args(["remote", "set-url", "origin", repo_url])
        .output()
        .await
        .map_err(|e| Error::Config(format!("git remote set-url: {e}")))?;
    if !set_url.status.success() {
        return Err(Error::Config(format!(
            "git remote set-url failed: {}",
            String::from_utf8_lossy(&set_url.stderr)
        )));
    }
    Ok(())
}

/// `git -C <host_path> checkout -b <branch> origin/<base_branch>`. The
/// full base ref string is constructed inside the helper.
async fn git_checkout_branch(host_path: &Path, branch: &str, base_branch: &str) -> Result<()> {
    let checkout = tokio::process::Command::new("git")
        .arg("-C")
        .arg(host_path)
        .args(["checkout", "-b", branch, &format!("origin/{base_branch}")])
        .output()
        .await
        .map_err(|e| Error::Config(format!("git checkout -b: {e}")))?;
    if !checkout.status.success() {
        return Err(Error::Config(format!(
            "git checkout -b {branch} from origin/{base_branch} failed: {}",
            String::from_utf8_lossy(&checkout.stderr)
        )));
    }
    Ok(())
}

/// Defense-in-depth: verify HEAD points at a real commit. If the checkout
/// silently produced an unborn HEAD (base_branch misspelled or unreachable
/// in the cloned base), reject the allocation here rather than letting the
/// coder commit an orphan that fails to merge later. On failure, removes
/// the cloned `host_path` before returning.
async fn git_verify_born_head(host_path: &Path, bare: &Path, base_branch: &str) -> Result<()> {
    let head_check = tokio::process::Command::new("git")
        .arg("-C")
        .arg(host_path)
        .arg("rev-parse")
        .arg("--verify")
        .arg("HEAD")
        .output()
        .await
        .map_err(|e| Error::Config(format!("git rev-parse HEAD: {e}")))?;
    if !head_check.status.success()
        || String::from_utf8_lossy(&head_check.stdout)
            .trim()
            .is_empty()
    {
        // No worktree linkage to clean up — just remove the cloned dir.
        let _ = tokio::fs::remove_dir_all(host_path).await;
        return Err(Error::Config(format!(
            "HEAD is unborn after checkout (base_branch={base_branch} \
             likely unreachable in bare clone {}); refusing to allocate",
            bare.display()
        )));
    }
    Ok(())
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
        // Compute bare path UPFRONT and take the per-bare-clone lock BEFORE
        // any git operation. This serializes concurrent fetch+worktree-add
        // against the same bare clone, eliminating the orphan-commit race
        // observed in A7v3/A7v4. Different repos still run concurrently.
        let (org, repo_name) = derive_org_repo(repo_url)?;
        let bare_path = root.join(".clones").join(&org).join(&repo_name);
        let lock = lock_for_bare(&bare_path);
        let _guard = lock.lock().await;

        let bare = ensure_bare_clone(repo_url, root).await?;
        debug_assert_eq!(bare, bare_path, "ensure_bare_clone path drift");
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
        git_local_clone(&bare, &host_path).await?;
        git_set_origin_url(&host_path, repo_url).await?;
        let branch = format!("auto/{}", brief_id.0);
        git_checkout_branch(&host_path, &branch, base_branch).await?;
        git_verify_born_head(&host_path, &bare, base_branch).await?;
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

/// Destroy a workspace iff `disposition` is `TearDown`; on `Preserve`, log
/// the retained path and return `Ok(())` without touching the dir. The daemon
/// derives the disposition from the brief's verdict string via
/// [`disposition_for`].
pub async fn destroy_with_disposition(
    ws: &BriefWorkspace,
    disposition: TerminationDisposition,
) -> Result<()> {
    match disposition {
        TerminationDisposition::TearDown => destroy(ws).await,
        TerminationDisposition::Preserve => {
            detach_worktree_branch(ws).await.unwrap_or_else(|e| {
                tracing::warn!(
                    workspace = %ws.host_path.display(),
                    error = %e,
                    "failed to detach worktree branch — subsequent fetches may collide"
                );
            });
            tracing::info!(
                "workspace preserved for forensics (worktree branch detached): {}",
                ws.host_path.display()
            );
            Ok(())
        }
    }
}

/// On `Preserve`, detach the worktree from its branch ref so subsequent
/// `git fetch` operations into the bare clone don't collide with
/// `refs/heads/auto/<brief_id>` checked out in the preserved worktree.
async fn detach_worktree_branch(ws: &BriefWorkspace) -> Result<()> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&ws.host_path)
        .args(["checkout", "--detach", "HEAD"])
        .output()
        .await
        .map_err(|e| Error::Config(format!("git checkout --detach HEAD spawn: {e}")))?;
    if !out.status.success() {
        return Err(Error::Config(format!(
            "git checkout --detach HEAD in {} failed: {}",
            ws.host_path.display(),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Filesystem record for one preserved per-brief workspace dir.
///
/// Produced by [`scan_workspaces`] and consumed by `agentry-workspace list/gc`.
#[derive(Debug, Clone)]
pub struct WorkspaceEntry {
    pub brief_id: String,
    pub path: PathBuf,
    pub age: std::time::Duration,
    pub disk_usage_bytes: u64,
    pub branch: Option<String>,
}

/// Walk `<root>/briefs/` and return one [`WorkspaceEntry`] per immediate
/// subdir. Errors reading any individual entry are skipped (the operator can
/// re-run if a transient ENOENT bites a single dir). Returns an empty Vec if
/// the briefs dir does not exist.
pub fn scan_workspaces(root: &Path) -> Vec<WorkspaceEntry> {
    let briefs_dir = root.join("briefs");
    let Ok(read) = std::fs::read_dir(&briefs_dir) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now();
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or(std::time::Duration::ZERO);
        out.push(WorkspaceEntry {
            brief_id: name.to_string(),
            path: path.clone(),
            age,
            disk_usage_bytes: dir_size(&path),
            branch: read_worktree_branch(&path),
        });
    }
    out.sort_by(|a, b| a.brief_id.cmp(&b.brief_id));
    out
}

/// Recursive byte-sum of every regular file under `dir`. Returns 0 on missing
/// dir or permission denied — the operator-facing column is best-effort.
fn dir_size(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            }
        }
    }
    total
}

/// Worktree's `.git` is a file containing `gitdir: <bare>/worktrees/<branch>`;
/// returns the trailing component as the branch hint, or None if absent.
fn read_worktree_branch(workspace: &Path) -> Option<String> {
    let head = workspace.join(".git");
    let raw = std::fs::read_to_string(&head).ok()?;
    let line = raw.lines().next()?;
    let suffix = line.strip_prefix("gitdir:")?.trim();
    let p = Path::new(suffix);
    p.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// One target of a GC run: a workspace dir plus its scan metadata.
#[derive(Debug, Clone)]
pub struct GcTarget {
    pub entry: WorkspaceEntry,
    pub removed: bool,
}

/// Run a GC pass: scan, filter to entries older than `threshold`, optionally
/// remove. Returns one [`GcTarget`] per matched entry. With `dry_run = true`
/// no dir is touched and `removed` is false on every entry.
pub fn gc_run(root: &Path, threshold: std::time::Duration, dry_run: bool) -> Vec<GcTarget> {
    let mut out = Vec::new();
    for entry in scan_workspaces(root) {
        if entry.age < threshold {
            continue;
        }
        let removed = if dry_run {
            false
        } else {
            std::fs::remove_dir_all(&entry.path).is_ok()
        };
        out.push(GcTarget { entry, removed });
    }
    out
}

/// Destroy a workspace. Safe on a nonexistent path (no-op).
///
/// Direct callers tear the workspace down unconditionally. The daemon should
/// route through [`destroy_with_disposition`] so a non-shipping verdict
/// preserves the dir for audit.
pub async fn destroy(ws: &BriefWorkspace) -> Result<()> {
    if !ws.host_path.exists() {
        return Ok(()); // idempotent — already gone
    }
    tokio::fs::remove_dir_all(&ws.host_path)
        .await
        .map_err(|e| Error::Config(format!("workspace destroy {}: {e}", ws.host_path.display())))
}

/// Sweep orphan `auto/*` branches from every bare clone under `<root>/.clones/`.
///
/// A branch is "orphan" iff its corresponding `<root>/briefs/<brief_id>` dir
/// no longer exists on disk. Branches whose worktree IS still present are
/// left alone — a brief may be in flight, or in `Preserve` disposition for
/// forensics. We don't second-guess that.
///
/// Idempotent and safe to run on every daemon boot. Returns the count of
/// branches deleted across all bare clones.
pub async fn sweep_orphan_branches(root: &Path) -> Result<usize> {
    let clones_dir = root.join(".clones");
    let briefs_dir = root.join("briefs");
    let mut total: usize = 0;

    let Ok(mut org_iter) = tokio::fs::read_dir(&clones_dir).await else {
        // No .clones dir yet (fresh root). Nothing to sweep.
        tracing::info!(swept = 0usize, "sweep_orphan_branches: no .clones dir");
        return Ok(0);
    };
    while let Ok(Some(org_entry)) = org_iter.next_entry().await {
        let org_path = org_entry.path();
        let Ok(meta) = org_entry.metadata().await else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Ok(mut repo_iter) = tokio::fs::read_dir(&org_path).await else {
            continue;
        };
        while let Ok(Some(repo_entry)) = repo_iter.next_entry().await {
            let bare = repo_entry.path();
            let Ok(meta) = repo_entry.metadata().await else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            // Confirm it's a populated bare clone (has HEAD).
            if tokio::fs::metadata(bare.join("HEAD")).await.is_err() {
                continue;
            }
            total += sweep_one_bare(&bare, &briefs_dir).await;
        }
    }
    tracing::info!(
        swept = total,
        "sweep_orphan_branches: deleted N orphan auto/* branches"
    );
    Ok(total)
}

/// Worker: sweep one bare clone. Returns the deletion count; per-branch
/// errors are logged and skipped so one bad branch can't abort the sweep.
async fn sweep_one_bare(bare: &Path, briefs_dir: &Path) -> usize {
    // Prune worktree admin entries whose worktree dir is gone (e.g. a legacy
    // brief whose dir was removed by hand). Without this, `git branch -D`
    // refuses to delete a branch still associated with a prunable worktree.
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare)
        .arg("worktree")
        .arg("prune")
        .output()
        .await;

    let listing = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare)
        .arg("for-each-ref")
        .arg("--format=%(refname:short)")
        .arg("refs/heads/auto/")
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::warn!(
                bare = %bare.display(),
                stderr = %String::from_utf8_lossy(&o.stderr),
                "for-each-ref failed; skipping bare"
            );
            return 0;
        }
        Err(e) => {
            tracing::warn!(
                bare = %bare.display(),
                error = %e,
                "for-each-ref spawn failed; skipping bare"
            );
            return 0;
        }
    };
    let stdout = String::from_utf8_lossy(&listing.stdout);

    let mut deleted: usize = 0;
    for line in stdout.lines() {
        let branch = line.trim();
        let Some(brief_id) = branch.strip_prefix("auto/") else {
            continue;
        };
        if brief_id.is_empty() {
            continue;
        }
        // Worktree still present → in-flight or Preserve disposition. Leave.
        if briefs_dir.join(brief_id).exists() {
            continue;
        }
        let del = tokio::process::Command::new("git")
            .arg("-C")
            .arg(bare)
            .arg("branch")
            .arg("-D")
            .arg(branch)
            .output()
            .await;
        match del {
            Ok(out) if out.status.success() => {
                tracing::debug!(
                    bare = %bare.display(),
                    branch = %branch,
                    "swept orphan auto/* branch"
                );
                deleted += 1;
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !(stderr.contains("not found") || stderr.contains("not a valid object")) {
                    tracing::warn!(
                        bare = %bare.display(),
                        branch = %branch,
                        stderr = %stderr,
                        "branch -D failed during sweep"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    bare = %bare.display(),
                    branch = %branch,
                    error = %e,
                    "branch -D spawn failed during sweep"
                );
            }
        }
    }
    deleted
}

/// Test-only hook on the private `bare_clone_locks` static. The lock
/// registry is otherwise process-private; the integration test for the
/// `unwrap_or_else(|poison| poison.into_inner())` recovery pattern
/// (mirroring PR #522 on `runtime_registry`) needs to poison the lock
/// from a panicking thread and then assert `lock_for_bare` survives.
/// Inline `#[cfg(test)] mod tests` in src/ is banned by
/// `arch-ban-inline-cfg-test-in-src`.
#[doc(hidden)]
pub fn __bare_clone_locks_for_test() -> &'static StdMutex<HashMap<PathBuf, Arc<TokioMutex<()>>>> {
    bare_clone_locks()
}

/// Test-only hook on the private `lock_for_bare` helper. See
/// [`__bare_clone_locks_for_test`] for rationale.
#[doc(hidden)]
pub fn __lock_for_bare_for_test(bare: &Path) -> Arc<TokioMutex<()>> {
    lock_for_bare(bare)
}
