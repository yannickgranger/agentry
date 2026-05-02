//! Integration tests for the role-directory JSON loader.
//!
//! Malformed-JSON propagation is covered by the pre-existing
//! `integration_role_loader_malformed.rs` against the same public surface;
//! we don't duplicate it here.

use orchestrator_runtime::redis_io;
use orchestrator_runtime::role_dir_loader::load_roles_from_dir;
use orchestrator_types::{
    AgentRole, PackageManager, PermitScope, RoleName, SubstrateClass, ToolAllowlist,
};
use redis::aio::ConnectionManager;
use std::path::{Path, PathBuf};

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn slug() -> String {
    format!(
        "rdl_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn minimal_role(name: &str) -> AgentRole {
    AgentRole {
        name: RoleName(name.into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "alpine:3.21".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: "#!/usr/bin/env bash\nexit 0\n".into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist::default(),
        allowed_tools: None,
        permit_scope: PermitScope::default(),
        passthru_env: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
        extra_bootstrap: vec![],
    }
}

fn write_role(dir: &Path, file_name: &str, role: &AgentRole) -> PathBuf {
    let p = dir.join(file_name);
    let body = serde_json::to_string_pretty(role).expect("ser role");
    std::fs::write(&p, body).expect("write role");
    p
}

/// Brief 190b of #182: the seed/roles directory at the workspace root must
/// contain JSON role files that deserialize cleanly as `AgentRole`. Pure
/// parse check — no Redis required.
#[test]
fn loads_seed_roles_dir_at_workspace_root() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root from CARGO_MANIFEST_DIR");
    let seed_roles = workspace_root.join("seed").join("roles");

    let commit_json = seed_roles.join("git-op-commit-v1.json");
    let push_json = seed_roles.join("git-op-push-v1.json");
    assert!(commit_json.exists(), "{}", commit_json.display());
    assert!(push_json.exists(), "{}", push_json.display());

    let commit_role: AgentRole =
        serde_json::from_str(&std::fs::read_to_string(&commit_json).expect("read commit JSON"))
            .expect("git-op-commit-v1.json deserialize");
    assert_eq!(commit_role.name.0, "git-op-commit");
    assert_eq!(commit_role.version, 1);
    assert!(commit_role
        .entrypoint_script
        .contains("exec /usr/local/bin/git-op-commit"));

    let push_role: AgentRole =
        serde_json::from_str(&std::fs::read_to_string(&push_json).expect("read push JSON"))
            .expect("git-op-push-v1.json deserialize");
    assert_eq!(push_role.name.0, "git-op-push");
    assert_eq!(push_role.version, 1);
    assert!(push_role
        .entrypoint_script
        .contains("exec /usr/local/bin/git-op-push"));
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn loads_empty_dir_returns_empty_vec() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");
    let result = load_roles_from_dir(&mut conn, Path::new("/nonexistent/path"))
        .await
        .expect("non-existent dir is OK");
    assert!(result.is_empty(), "missing dir must yield empty Vec");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn loads_two_role_jsons_returns_both_names() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");

    let s = slug();
    let n_a = format!("zz-rdl-a-{s}");
    let n_b = format!("zz-rdl-b-{s}");
    let dir = tempfile::tempdir().expect("tempdir");
    write_role(dir.path(), "a.json", &minimal_role(&n_a));
    write_role(dir.path(), "b.json", &minimal_role(&n_b));

    let names = load_roles_from_dir(&mut conn, dir.path())
        .await
        .expect("load");
    assert_eq!(names.len(), 2);
    assert_eq!(names[0].0, n_a);
    assert_eq!(names[1].0, n_b);

    let fetched_a = redis_io::fetch_role(&mut conn, &RoleName(n_a.clone()), 1)
        .await
        .expect("fetch a");
    assert_eq!(fetched_a.name.0, n_a);

    use redis::AsyncCommands;
    let _: () = conn
        .del::<_, ()>(format!("agentry:role:{n_a}:v1"))
        .await
        .expect("cleanup a");
    let _: () = conn
        .del::<_, ()>(format!("agentry:role:{n_b}:v1"))
        .await
        .expect("cleanup b");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn loads_in_alphabetical_order() {
    let Some(url) = test_redis_url() else { return };
    let mut conn: ConnectionManager = redis_io::connect(&url).await.expect("connect");

    let s = slug();
    let n_a = format!("zz-rdl-ord-a-{s}");
    let n_b = format!("zz-rdl-ord-b-{s}");
    let n_c = format!("zz-rdl-ord-c-{s}");
    let dir = tempfile::tempdir().expect("tempdir");
    write_role(dir.path(), "c.json", &minimal_role(&n_c));
    write_role(dir.path(), "a.json", &minimal_role(&n_a));
    write_role(dir.path(), "b.json", &minimal_role(&n_b));

    let names = load_roles_from_dir(&mut conn, dir.path())
        .await
        .expect("load");
    let collected: Vec<String> = names.iter().map(|n| n.0.clone()).collect();
    assert_eq!(collected, vec![n_a.clone(), n_b.clone(), n_c.clone()]);

    use redis::AsyncCommands;
    for n in [n_a, n_b, n_c] {
        let _: () = conn
            .del::<_, ()>(format!("agentry:role:{n}:v1"))
            .await
            .expect("cleanup");
    }
}
