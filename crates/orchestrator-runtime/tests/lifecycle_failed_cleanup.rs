//! Stale-worktree GC on terminal `BriefState::Failed` (L.4 / EPIC #246).
//!
//! Pre-L.4 the daemon retained the worktree dir on every failed brief
//! "for audit". That rule produced ~6 dispatch-blocking stale-worktree
//! incidents in the EPIC #255/#256 drain — failures are recoverable
//! from Redis (trace stream + state log), the on-disk worktree is not
//! load-bearing for the audit trail. L.4 moves cleanup into the FSM
//! driver so it fires whenever the FSM observes the terminal Failed
//! transition, regardless of which path got the brief there.
//!
//! These tests focus on `lifecycle_driver::cleanup_failed_brief_at` in
//! isolation per the brief's recommendation: it pins the cleanup
//! contract (worktree + auto/<brief_id> branch removed; idempotent on
//! second invocation; safe on a non-existent brief). The end-to-end
//! `projector_task` Failed-cleanup path is covered indirectly via the
//! cleanup function itself — the projector_task wires cleanup into the
//! same code path under a `matches!(state, BriefState::Failed { .. })`
//! gate.

use async_trait::async_trait;
use orchestrator_runtime::lifecycle::{
    EventSource, EventSourceError, StateProjector, StateProjectorError,
};
use orchestrator_runtime::lifecycle_driver::{cleanup_failed_brief_at, projector_task};
use orchestrator_runtime::workspace::{allocate_at, BriefWorkspace};
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord, Reason};
use orchestrator_types::BriefId;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn brief(id: &str) -> BriefId {
    BriefId(id.into())
}

/// Run a real `git` command in `cwd`, panicking on failure with the git
/// stderr included. Mirrors `workspace_test::run_git`.
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

/// Set up a bare upstream repo with one commit on `main`. Returns the
/// `file://` URL usable as `repo_url` for `allocate_at`. Mirrors
/// `workspace_test::setup_upstream`.
async fn setup_upstream(dir: &Path) -> String {
    let upstream = dir.join("upstream.git");
    tokio::fs::create_dir_all(&upstream)
        .await
        .expect("mk upstream dir");
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

/// Mirrors `workspace_test::bare_clone_path_for`: derive the bare-clone
/// dir for the temp-root layout used by these tests.
fn bare_clone_path_for(root: &Path) -> std::path::PathBuf {
    let parent = root
        .file_name()
        .expect("tmp dir has a name")
        .to_string_lossy()
        .into_owned();
    root.join(".clones").join(parent).join("upstream")
}

/// Whether `branch` is present in the bare clone at `bare`.
async fn branch_exists(bare: &Path, branch: &str) -> bool {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .await
        .expect("git show-ref spawn");
    out.status.success()
}

/// Scenario 1: cleanup against a real worktree allocation removes both
/// the worktree directory and the `auto/<brief_id>` branch from the
/// bare clone. The bare clone itself survives — it's shared across
/// briefs.
#[tokio::test]
async fn cleanup_removes_worktree_dir_and_auto_branch() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;
    let bid = brief("brf_failed_cleanup_real");

    let ws = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc");
    assert!(ws.host_path.exists(), "worktree dir must exist pre-cleanup");

    let bare = bare_clone_path_for(tmp.path());
    let auto_branch = format!("auto/{}", bid.0);
    assert!(
        branch_exists(&bare, &auto_branch).await,
        "auto/<brief_id> must exist in bare clone pre-cleanup"
    );

    cleanup_failed_brief_at(&bid, tmp.path(), None).await;

    assert!(
        !ws.host_path.exists(),
        "cleanup must remove the worktree dir"
    );
    assert!(
        !branch_exists(&bare, &auto_branch).await,
        "cleanup must delete the auto/<brief_id> branch from the bare clone"
    );
    assert!(
        bare.join("HEAD").exists(),
        "cleanup must NOT nuke the shared bare clone"
    );
}

/// Scenario 2: idempotency. Driving cleanup twice against the same
/// brief must not panic, must not return an error (cleanup is
/// best-effort by contract), and must leave the post-cleanup invariants
/// intact (worktree gone, branch gone). This is the regression for
/// "FSM transitions to Failed twice" — re-entry, replay, or a future
/// retry path that lands back on Failed must all be safe.
#[tokio::test]
async fn cleanup_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;
    let bid = brief("brf_failed_cleanup_idem");

    let ws = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc");
    let bare = bare_clone_path_for(tmp.path());
    let auto_branch = format!("auto/{}", bid.0);

    cleanup_failed_brief_at(&bid, tmp.path(), None).await;
    // First cleanup tore down the worktree + branch.
    assert!(!ws.host_path.exists());
    assert!(!branch_exists(&bare, &auto_branch).await);

    // Second cleanup is a no-op: nothing to remove, nothing to error.
    cleanup_failed_brief_at(&bid, tmp.path(), None).await;
    assert!(!ws.host_path.exists());
    assert!(!branch_exists(&bare, &auto_branch).await);
}

/// Scenario 3: cleanup against a brief that never had a workspace
/// allocated is a no-op — the FSM may transition to Failed without ever
/// having stood up a worktree (e.g. a probe brief, or a brief that
/// failed during dispatch). The cleanup function must not error.
#[tokio::test]
async fn cleanup_on_never_allocated_brief_is_noop() {
    let tmp = tempfile::tempdir().expect("tmp");
    let bid = brief("brf_never_allocated");

    let host_path = tmp.path().join("briefs").join(&bid.0);
    assert!(!host_path.exists(), "precondition: brief dir absent");

    cleanup_failed_brief_at(&bid, tmp.path(), None).await;

    assert!(
        !host_path.exists(),
        "cleanup must leave the never-allocated brief absent"
    );
}

// ---------------------------------------------------------------------------
// FSM-driven cleanup: drive `projector_task` with an event sequence that
// terminates in `Failed{BudgetExhausted}` and assert the worktree is gone
// post-projection. The verdict-conn is `None` so the test does not need
// Redis — the cleanup branch fires unconditionally on terminal Failed.
// ---------------------------------------------------------------------------

struct MemEventSource {
    events: VecDeque<BriefEvent>,
}

#[async_trait]
impl EventSource for MemEventSource {
    async fn next(&mut self) -> Result<Option<BriefEvent>, EventSourceError> {
        Ok(self.events.pop_front())
    }
}

struct MemStateProjector {
    written: Arc<Mutex<Vec<(BriefStateRecord, String)>>>,
}

#[async_trait]
impl StateProjector for MemStateProjector {
    async fn write(
        &mut self,
        record: &BriefStateRecord,
        last_trace_id: &str,
    ) -> Result<(), StateProjectorError> {
        self.written
            .lock()
            .expect("mutex")
            .push((record.clone(), last_trace_id.to_string()));
        Ok(())
    }
}

/// Drive `projector_task` to `Failed{BudgetExhausted}` via the universal
/// `BudgetExhausted` handler. With `AGENTRY_WORKSPACE_ROOT` pointed at a
/// temp dir holding a synthetic worktree, the projector_task's
/// terminal-Failed cleanup must remove the worktree.
///
/// This test uses `serial_test` patterns implicitly by virtue of being
/// the only test in this file that touches the env var — other cases
/// here use the explicit-root variant. Within this file the test runs
/// alone so the env var read is uncontested.
#[tokio::test]
async fn projector_task_cleans_up_on_terminal_failed() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;
    let bid = brief("brf_fsm_failed_cleanup");

    let ws: BriefWorkspace = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc");
    assert!(ws.host_path.exists());

    // The default `cleanup_failed_brief` resolves the root via
    // `BriefWorkspace::root()` (env var). Point it at this test's tmp
    // root so the projector_task's terminal-Failed branch finds the
    // worktree we just allocated. Other tests in this file use the
    // explicit-root `cleanup_failed_brief_at` variant and don't read the
    // env var, so the global mutation here doesn't race them.
    std::env::set_var("AGENTRY_WORKSPACE_ROOT", tmp.path());

    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource {
        events: VecDeque::from(vec![BriefEvent::BudgetExhausted]),
    });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    projector_task(bid.clone(), source, projector, None)
        .await
        .expect("projector_task");

    let log = written.lock().expect("mutex").clone();
    assert_eq!(log.len(), 1, "one record per legal transition");
    match &log[0].0.state {
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        } => {}
        other => panic!("expected Failed{{BudgetExhausted}}, got {other:?}"),
    }

    assert!(
        !ws.host_path.exists(),
        "projector_task must invoke cleanup on terminal Failed"
    );

    std::env::remove_var("AGENTRY_WORKSPACE_ROOT");
}
