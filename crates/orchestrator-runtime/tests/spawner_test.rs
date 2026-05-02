//! Public-surface smoke tests for the spawner.
//!
//! The bulk of `spawner.rs`'s prior unit tests exercised private helpers
//! (`compute_verdict`, `bootstrap_command`, `effective_binaries`,
//! `coder_tool_mount_*`, `preflight_transcripts_mount`, `brief_env_args`,
//! `PodmanSpawner::container_name`). The migration recipe forbids
//! promoting their visibility, so those tests are dropped — the behaviour
//! they covered is exercised end-to-end by the daemon integration tests.

use orchestrator_runtime::spawner::{kill, workspace_path, PodmanSpawner};
use orchestrator_runtime::Error;
use orchestrator_types::BriefId;

#[test]
fn podman_spawner_constructs() {
    let _ = PodmanSpawner::new();
}

#[test]
fn workspace_path_returns_none_for_unregistered_brief() {
    let bid = BriefId("brf_does_not_exist".into());
    assert!(workspace_path(&bid).is_none());
}

#[tokio::test]
async fn kill_returns_not_found_for_unregistered_brief() {
    let bid = BriefId("brf_kill_unregistered".into());
    let err = kill(&bid).await.expect_err("must error");
    match err {
        Error::NotFound { kind, key } => {
            assert_eq!(kind, "running container");
            assert_eq!(key, "brf_kill_unregistered");
        }
        other => panic!("expected Error::NotFound, got {other:?}"),
    }
}
