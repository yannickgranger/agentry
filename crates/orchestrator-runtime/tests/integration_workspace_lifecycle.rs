//! Workspace lifecycle: verdict-driven teardown vs forensic preservation,
//! plus the GC pass that sweeps old preserved dirs.
//!
//! The GC tests call the pure `workspace::gc_run` rather than shelling out to
//! the `agentry-workspace` binary — the bin is a thin wrapper, and pure-fn
//! calls keep the tests deterministic across CI runners that may not have a
//! built binary on PATH.

use orchestrator_runtime::workspace::{
    self, allocate_at, destroy_with_disposition, disposition_for, gc_run, scan_workspaces,
    BriefWorkspace, TerminationDisposition,
};
use orchestrator_types::BriefId;
use std::time::Duration;

fn brief(id: &str) -> BriefId {
    BriefId(id.into())
}

#[test]
fn disposition_for_matrix() {
    // The full disposition table from issue #95.
    assert_eq!(disposition_for("shipped"), TerminationDisposition::TearDown);
    assert_eq!(
        disposition_for("review-blocked"),
        TerminationDisposition::TearDown
    );
    assert_eq!(
        disposition_for("review-blocked: ci-red"),
        TerminationDisposition::TearDown
    );
    assert_eq!(
        disposition_for("review-blocked-pending-human"),
        TerminationDisposition::TearDown
    );
    assert_eq!(
        disposition_for("failed: acceptance"),
        TerminationDisposition::Preserve
    );
    assert_eq!(
        disposition_for("failed: claude-timeout"),
        TerminationDisposition::Preserve
    );
    assert_eq!(
        disposition_for("failed: stalled"),
        TerminationDisposition::Preserve
    );
    assert_eq!(
        disposition_for("failed: spawner-error"),
        TerminationDisposition::Preserve
    );
    // Unknown verdict — default-safe to Preserve.
    assert_eq!(
        disposition_for("some-future-verdict"),
        TerminationDisposition::Preserve
    );
    assert_eq!(disposition_for(""), TerminationDisposition::Preserve);
}

#[tokio::test]
async fn shipped_verdict_destroys_workspace() {
    let tmp = tempfile::tempdir().expect("tmp");
    let ws = allocate_at(&brief("brf_ship_destroy"), None, tmp.path())
        .await
        .expect("alloc");
    assert!(ws.host_path.exists(), "workspace dir created");

    let disp = disposition_for("shipped");
    assert_eq!(disp, TerminationDisposition::TearDown);

    destroy_with_disposition(&ws, disp).await.expect("destroy");
    assert!(
        !ws.host_path.exists(),
        "shipped → TearDown must remove the dir"
    );
}

#[tokio::test]
async fn claude_timeout_preserves_workspace() {
    let tmp = tempfile::tempdir().expect("tmp");
    let ws = allocate_at(&brief("brf_timeout_preserve"), None, tmp.path())
        .await
        .expect("alloc");
    let canary = ws.host_path.join("canary");
    tokio::fs::write(&canary, b"forensic-evidence")
        .await
        .expect("write canary");

    let disp = disposition_for("failed: claude-timeout");
    assert_eq!(disp, TerminationDisposition::Preserve);

    destroy_with_disposition(&ws, disp)
        .await
        .expect("preserve path returns Ok");
    assert!(
        ws.host_path.exists(),
        "claude-timeout → Preserve must keep the dir on disk for forensics"
    );
    assert!(
        canary.exists(),
        "Preserve must not touch dir contents — canary file should survive"
    );
}

/// Build a fake briefs root with two preserved workspace dirs: one whose mtime
/// is `old_age` ago and one freshly created. Returns the temp root + the two
/// workspace paths.
fn seed_two_workspaces(
    old_age: Duration,
) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tmp");
    let briefs = tmp.path().join("briefs");
    std::fs::create_dir_all(&briefs).expect("mk briefs");

    let old = briefs.join("brf_old");
    let fresh = briefs.join("brf_fresh");
    std::fs::create_dir(&old).expect("mk old");
    std::fs::create_dir(&fresh).expect("mk fresh");
    std::fs::write(old.join("scratch"), b"x").expect("seed old");
    std::fs::write(fresh.join("scratch"), b"x").expect("seed fresh");

    // Backdate the old dir's mtime so gc thresholds resolve deterministically.
    let now = std::time::SystemTime::now();
    let backdated = now - old_age;
    let backdated_ft = filetime::FileTime::from_system_time(backdated);
    filetime::set_file_mtime(&old, backdated_ft).expect("set mtime");

    (tmp, old, fresh)
}

#[test]
fn gc_dry_run_lists_targets_no_removal() {
    // Old dir's mtime = 60s ago; threshold = 1s; dry run.
    let (tmp, old, fresh) = seed_two_workspaces(Duration::from_secs(60));

    let targets = gc_run(tmp.path(), Duration::from_secs(1), true);
    let ids: Vec<&str> = targets.iter().map(|t| t.entry.brief_id.as_str()).collect();
    assert!(
        ids.contains(&"brf_old"),
        "old workspace should be flagged: ids={ids:?}"
    );
    assert!(
        targets.iter().all(|t| !t.removed),
        "dry-run must not actually remove"
    );

    assert!(old.exists(), "dry-run preserves old dir on disk");
    assert!(fresh.exists(), "dry-run preserves fresh dir on disk");
}

#[test]
fn gc_removes_old_keeps_new() {
    let (tmp, old, fresh) = seed_two_workspaces(Duration::from_secs(60));

    let targets = gc_run(tmp.path(), Duration::from_secs(1), false);
    let ids: Vec<&str> = targets.iter().map(|t| t.entry.brief_id.as_str()).collect();
    assert_eq!(ids, vec!["brf_old"], "only old should be a target");
    assert!(
        targets.iter().all(|t| t.removed),
        "non-dry-run must actually remove"
    );

    assert!(!old.exists(), "old dir removed by gc");
    assert!(fresh.exists(), "fresh dir kept");
}

#[test]
fn scan_workspaces_skips_files_and_missing_root() {
    // Empty root: scan returns Vec::new without erroring.
    let tmp = tempfile::tempdir().expect("tmp");
    assert!(scan_workspaces(tmp.path()).is_empty());

    // Create briefs/, drop a regular file alongside dirs, ensure files are skipped.
    let briefs = tmp.path().join("briefs");
    std::fs::create_dir_all(&briefs).expect("mk briefs");
    std::fs::write(briefs.join("not-a-workspace"), b"junk").expect("write file");
    std::fs::create_dir(briefs.join("brf_real")).expect("mk dir");

    let entries = scan_workspaces(tmp.path());
    let ids: Vec<&str> = entries.iter().map(|e| e.brief_id.as_str()).collect();
    assert_eq!(ids, vec!["brf_real"]);
}

#[tokio::test]
async fn legacy_destroy_still_tears_down() {
    // Existing test-only call sites that pass TerminationDisposition::TearDown
    // continue to behave like the legacy `destroy` (which is now a private
    // implementation detail of `destroy_with_disposition`).
    let tmp = tempfile::tempdir().expect("tmp");
    let ws: BriefWorkspace = allocate_at(&brief("brf_legacy_td"), None, tmp.path())
        .await
        .expect("alloc");
    workspace::destroy_with_disposition(&ws, TerminationDisposition::TearDown)
        .await
        .expect("td");
    assert!(!ws.host_path.exists());
}
