//! Public-surface tests for the per-brief workspace allocator.
//!
//! The prior inline tests for the private `derive_org_repo` helper and the
//! private `bare_clone_path_for_worktree` helper are dropped: the migration
//! recipe forbids promoting their visibility, and both helpers are
//! exercised through `allocate_at` / `destroy` end-to-end below. Tests
//! that need the bare-clone path on disk recompute it from the test's
//! known upstream layout (URL is always `file://<tmp>/upstream.git`,
//! so the bare lands at `<tmp>/.clones/<tmp_basename>/upstream`).

use orchestrator_runtime::workspace::{
    allocate_at, destroy, destroy_with_disposition, ensure_bare_clone, sweep_orphan_branches,
    BriefWorkspace, TerminationDisposition,
};
use orchestrator_types::BriefId;
use std::path::{Path, PathBuf};

fn brief(id: &str) -> BriefId {
    BriefId(id.into())
}

/// Mirror of the production `derive_org_repo` layout, specialized for the
/// `setup_upstream` helper below: `file://<tmp>/upstream.git` resolves to
/// `<root>/.clones/<tmp_basename>/upstream`.
fn bare_clone_path_for(root: &Path) -> PathBuf {
    let parent = root
        .file_name()
        .expect("tmp dir has a name")
        .to_string_lossy()
        .into_owned();
    root.join(".clones").join(parent).join("upstream")
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
    let bare_dir = bare_clone_path_for(tmp.path());
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

/// Regression for the orphan-commit race that bit A7v3 (PR #61) and A7v4
/// (PR #68): when N concurrent allocations target the same bare clone,
/// a `git fetch --prune` on one task could leave `refs/heads/<base>`
/// transiently unborn while another task's `git worktree add ... <base>`
/// resolved it, producing a worktree with unborn HEAD. The coder's commit
/// in such a worktree had no parent — the forge later rejected the merge
/// with "fatal: refusing to merge unrelated histories".
///
/// With the per-bare-clone async lock, every concurrent allocation lands
/// a worktree whose HEAD points at a real commit on the base branch.
#[tokio::test]
async fn concurrent_allocate_against_same_repo_no_orphan_heads() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;
    let root = tmp.path().to_path_buf();

    // Spawn 5 concurrent allocations against the same bare clone.
    let mut handles = Vec::new();
    for i in 0..5 {
        let url = url.clone();
        let root = root.clone();
        handles.push(tokio::spawn(async move {
            let bid = brief(&format!("brf_concur_{i}"));
            allocate_at(&bid, Some((url.as_str(), "main")), &root).await
        }));
    }

    let mut workspaces = Vec::new();
    for h in handles {
        let ws = h.await.expect("join").expect("alloc must succeed");
        workspaces.push(ws);
    }
    assert_eq!(workspaces.len(), 5, "all concurrent allocations succeed");

    // Every worktree's HEAD must point at a real commit (no unborn HEAD).
    for ws in &workspaces {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&ws.host_path)
            .arg("rev-parse")
            .arg("--verify")
            .arg("HEAD")
            .output()
            .await
            .expect("git rev-parse HEAD");
        assert!(
            out.status.success(),
            "rev-parse HEAD failed for {}: {}",
            ws.host_path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            !sha.is_empty(),
            "worktree {} has empty HEAD — orphan-prone state",
            ws.host_path.display()
        );
        // And HEAD's commit must have NO empty parent line. We test this
        // by reading the commit object: a normal commit has at least one
        // `parent <sha>` line OR is a root commit reachable from main.
        // Stronger assertion: the commit equals or is reachable from main.
        // The bare clone uses the `+refs/heads/*:refs/heads/*` refspec, so
        // `main` (not `origin/main`) is the canonical local ref.
        let merge_base = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&ws.host_path)
            .arg("merge-base")
            .arg("HEAD")
            .arg("main")
            .output()
            .await
            .expect("git merge-base");
        assert!(
            merge_base.status.success(),
            "no merge-base between worktree HEAD and main for {} — \
                 worktree was created from an unborn ref (orphan-commit race): {}",
            ws.host_path.display(),
            String::from_utf8_lossy(&merge_base.stderr)
        );
        let mb_sha = String::from_utf8_lossy(&merge_base.stdout)
            .trim()
            .to_string();
        assert!(
            !mb_sha.is_empty(),
            "merge-base returned empty SHA for {}",
            ws.host_path.display()
        );
    }
}

/// Returns true iff `refs/heads/<branch>` exists in the bare clone at `bare`.
async fn branch_exists(bare: &Path, branch: &str) -> bool {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare)
        .arg("show-ref")
        .arg("--verify")
        .arg("--quiet")
        .arg(format!("refs/heads/{branch}"))
        .output()
        .await
        .expect("git show-ref spawn");
    out.status.success()
}

/// Pin: `destroy(&ws)` must delete the per-brief `auto/<brief_id>` branch
/// from the bare clone. Without this the bare accumulates stale refs that
/// eventually collide with future dispatches (production hit ~101 stale
/// branches; dispatch fails with "branch already exists").
#[tokio::test]
async fn destroy_deletes_branch_ref() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;
    let bid = brief("test-brief-1");
    let ws = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc");

    let bare = bare_clone_path_for(tmp.path());

    assert!(
        branch_exists(&bare, "auto/test-brief-1").await,
        "branch ref must exist before destroy"
    );

    destroy(&ws).await.expect("destroy");

    assert!(
        !branch_exists(&bare, "auto/test-brief-1").await,
        "branch ref must be gone after destroy"
    );
}

/// `sweep_orphan_branches` MUST delete only branches whose corresponding
/// `briefs/<brief_id>` dir is missing, leaving in-flight worktrees alone.
#[tokio::test]
async fn sweep_orphan_branches_removes_only_orphans() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;

    let keeper = brief("keeper");
    let orphan = brief("orphan");
    let _keeper_ws = allocate_at(&keeper, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc keeper");
    let orphan_ws = allocate_at(&orphan, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc orphan");

    // Simulate the legacy stale-branch-no-worktree state: the briefs dir
    // is gone but the bare-clone branch ref + worktree admin entry
    // remain. The sweep must reconcile via `git worktree prune` then
    // delete the now-truly-orphan ref.
    tokio::fs::remove_dir_all(&orphan_ws.host_path)
        .await
        .expect("remove orphan briefs dir");

    let bare = bare_clone_path_for(tmp.path());
    assert!(branch_exists(&bare, "auto/keeper").await);
    assert!(branch_exists(&bare, "auto/orphan").await);

    let count = sweep_orphan_branches(tmp.path()).await.expect("sweep");
    assert_eq!(count, 1, "exactly one orphan branch deleted");

    assert!(
        branch_exists(&bare, "auto/keeper").await,
        "keeper branch must survive (its worktree dir is still on disk)"
    );
    assert!(
        !branch_exists(&bare, "auto/orphan").await,
        "orphan branch must be gone"
    );
}

/// Sweep must be a no-op (and not error) when the root has no `.clones`
/// dir yet — relevant on a freshly-deployed daemon's first boot.
#[tokio::test]
async fn sweep_orphan_branches_handles_missing_clones_dir() {
    let tmp = tempfile::tempdir().expect("tmp");
    let count = sweep_orphan_branches(tmp.path())
        .await
        .expect("sweep on empty root");
    assert_eq!(count, 0);
}

/// On `Preserve`, the worktree's `auto/<brief_id>` branch must be detached
/// so subsequent `git fetch` calls into the bare clone don't refuse with
/// "refusing to fetch into branch ... checked out at <path>".
#[tokio::test]
async fn preserve_detaches_worktree_branch() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;
    let bid = brief("brf_preserve_detach");
    let ws = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc");

    let bare = bare_clone_path_for(tmp.path());
    let branch = format!("auto/{}", bid.0);

    // Reproduce the pre-fix failure mode: give the upstream a divergent
    // `auto/<brief_id>` ref so the bare's next `git fetch` will try to
    // fast-forward the local ref — which the worktree blocks.
    let seed = tmp.path().join("seed");
    run_git(&seed, &["checkout", "-q", "-b", branch.as_str()]).await;
    tokio::fs::write(seed.join("CHANGE"), b"diverge\n")
        .await
        .expect("write change");
    run_git(&seed, &["add", "CHANGE"]).await;
    run_git(&seed, &["commit", "-q", "-m", "diverge"]).await;
    let upstream = tmp.path().join("upstream.git");
    let push = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&seed)
        .args(["push", "-q"])
        .arg(&upstream)
        .arg(&branch)
        .output()
        .await
        .expect("push spawn");
    assert!(
        push.status.success(),
        "seed push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    // Pre-condition: fetch in the bare must fail with the expected collision.
    let pre = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&bare)
        .arg("fetch")
        .output()
        .await
        .expect("pre-fetch spawn");
    assert!(
        !pre.status.success(),
        "pre-detach fetch was expected to fail: worktree has the branch checked out"
    );
    let pre_stderr = String::from_utf8_lossy(&pre.stderr);
    assert!(
        pre_stderr.contains("checked out"),
        "expected checkout collision in stderr, got: {pre_stderr}"
    );

    // Preserve must detach the worktree and leave the dir on disk.
    destroy_with_disposition(&ws, TerminationDisposition::Preserve)
        .await
        .expect("destroy_with_disposition Preserve");
    assert!(
        ws.host_path.exists(),
        "worktree dir must survive Preserve disposition"
    );

    // The same fetch must now succeed.
    let post = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&bare)
        .arg("fetch")
        .output()
        .await
        .expect("post-fetch spawn");
    assert!(
        post.status.success(),
        "post-detach fetch must succeed: {}",
        String::from_utf8_lossy(&post.stderr)
    );
}

/// `detach_worktree_branch` failing (e.g. workspace dir is missing) must
/// not propagate — the Preserve arm logs a warning and returns Ok so the
/// daemon continues to record the retain.
#[tokio::test]
async fn preserve_logs_warning_on_detach_failure() {
    let tmp = tempfile::tempdir().expect("tmp");
    let ws = BriefWorkspace {
        brief_id: brief("brf_preserve_no_dir"),
        host_path: tmp.path().join("does_not_exist"),
    };
    destroy_with_disposition(&ws, TerminationDisposition::Preserve)
        .await
        .expect("Preserve must return Ok even when detach fails");
}
