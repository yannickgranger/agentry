//! Public-surface smoke tests for the spawner.
//!
//! The bulk of `spawner.rs`'s prior unit tests exercised private helpers
//! (`compute_verdict`, `bootstrap_command`, `effective_binaries`,
//! `coder_tool_mount_*`, `preflight_transcripts_mount`, `brief_env_args`,
//! `PodmanSpawner::container_name`). The migration recipe forbids
//! promoting their visibility, so those tests are dropped — the behaviour
//! they covered is exercised end-to-end by the daemon integration tests.
//!
//! Issue #506 Option B (agency.lab SSL bypass) is exercised here against
//! the peer-visibility helpers `forge_host_is_agency_lab` and
//! `agency_lab_ssl_bypass_env_args`: the arch ban on inline `#[cfg(test)]`
//! items in `src/` forces the helpers to be promoted to `pub` and tested
//! from this file. Asserting the actual `cmd.arg(...)` sequence on
//! `tokio::process::Command` is not feasible — the type provides no
//! introspection API — so the test surface is the env-args helper, and
//! the call-site in `run_agent` is a one-liner over its return value.

use orchestrator_runtime::spawner::{
    agency_lab_ssl_bypass_env_args, forge_host_is_agency_lab, kill, workspace_path, PodmanSpawner,
};
use orchestrator_runtime::Error;
use orchestrator_types::{Brief, BriefId, VersionedRef};

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

fn brief_with_payload(payload: serde_json::Value) -> Brief {
    Brief::new(
        "test",
        VersionedRef {
            name: "topo".into(),
            version: 1,
        },
        payload,
    )
}

#[test]
fn forge_host_is_agency_lab_matches_with_and_without_port() {
    assert!(forge_host_is_agency_lab("agency.lab"));
    assert!(forge_host_is_agency_lab("agency.lab:3000"));
    assert!(forge_host_is_agency_lab("agency.lab:80"));
}

#[test]
fn forge_host_is_agency_lab_rejects_other_hosts() {
    assert!(!forge_host_is_agency_lab("github.com"));
    assert!(!forge_host_is_agency_lab("github.com:443"));
    assert!(!forge_host_is_agency_lab("agency.lab.evil.com"));
    assert!(!forge_host_is_agency_lab(""));
}

#[test]
fn agency_lab_brief_receives_ssl_bypass_env() {
    let brief = brief_with_payload(serde_json::json!({
        "target_repo": "yg/qbot-core",
        "forge_host": "agency.lab:3000",
        "base_branch": "develop",
    }));
    let args = agency_lab_ssl_bypass_env_args(&brief);
    assert!(
        args.iter().any(|a| a == "GIT_SSL_NO_VERIFY=1"),
        "expected GIT_SSL_NO_VERIFY=1, got {args:?}",
    );
    assert!(
        args.iter()
            .any(|a| a == "CARGO_NET_GIT_FETCH_WITH_CLI=true"),
        "expected CARGO_NET_GIT_FETCH_WITH_CLI=true, got {args:?}",
    );
}

#[test]
fn agency_lab_brief_without_port_still_receives_ssl_bypass_env() {
    let brief = brief_with_payload(serde_json::json!({
        "target_repo": "yg/agentry",
        "forge_host": "agency.lab",
    }));
    let args = agency_lab_ssl_bypass_env_args(&brief);
    assert_eq!(args.len(), 2);
}

#[test]
fn non_agency_forge_brief_does_not_receive_ssl_bypass_env() {
    let brief = brief_with_payload(serde_json::json!({
        "target_repo": "octocat/hello-world",
        "forge_host": "github.com",
    }));
    let args = agency_lab_ssl_bypass_env_args(&brief);
    assert!(
        args.is_empty(),
        "non-agency forge must not get SSL bypass env; got {args:?}",
    );
}

#[test]
fn missing_forge_host_does_not_receive_ssl_bypass_env() {
    let brief = brief_with_payload(serde_json::json!({
        "target_repo": "yg/agentry",
    }));
    let args = agency_lab_ssl_bypass_env_args(&brief);
    assert!(args.is_empty(), "no forge_host means no injection");
}
