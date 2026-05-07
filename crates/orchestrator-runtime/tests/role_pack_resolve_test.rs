//! Integration tests for `spawner::resolve_role_with_packs` (slice I/1c).
//!
//! Live-Redis tests gate on `AGENTRY_TEST_REDIS_URL` and stay `#[ignore]` so
//! the workspace-wide `cargo test` pass stays green without a Redis
//! dependency — same convention as `tests/redis_io_test.rs` and
//! `tests/tool_pack_seed_test.rs`. The pure no-Redis case (empty
//! `tool_packs` short-circuits to a clone) is verified without touching
//! Redis below.

use orchestrator_runtime::redis_io;
use orchestrator_runtime::spawner::resolve_role_with_packs;
use orchestrator_types::{
    AgentRole, PackageManager, PermitScope, RoleName, SubstrateClass, ToolAllowlist, ToolPack,
};

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn slug() -> String {
    format!(
        "rpr_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn minimal_role(tool_packs: Vec<String>) -> AgentRole {
    AgentRole {
        name: RoleName("resolve-probe".into()),
        version: 1,
        model: None,
        system_prompt: Some("base prompt".into()),
        image: "alpine:3.21".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: "echo body".into(),
        exitpoint_script: None,
        binaries: vec!["git".into()],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist::default(),
        allowed_tools: None,
        permit_scope: PermitScope::default(),
        passthru_env: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
        extra_bootstrap: vec![],
        tool_packs,
    }
}

/// Pure (no-Redis) sub-case: empty `tool_packs` short-circuits to a clone
/// and never touches the connection. We still need a connection to satisfy
/// the signature, so we skip when the URL is unset.
#[tokio::test]
async fn resolve_role_with_packs_empty_returns_clone_no_redis_needed() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = redis_io::connect(&url).await.expect("connect");
    let role = minimal_role(vec![]);
    let resolved = resolve_role_with_packs(&role, &mut conn)
        .await
        .expect("empty tool_packs must short-circuit to a clone");
    assert_eq!(
        resolved, role,
        "empty tool_packs must produce a structural clone of the role",
    );
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn resolve_role_with_packs_picks_latest_seeded_version() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");

    let s = slug();
    let pack_name = format!("zz-resolve-pack-{s}");

    let v1 = ToolPack {
        name: pack_name.clone(),
        version: 1,
        binaries: vec!["v1-bin".into()],
        container_bootstrap: vec!["v1-bootstrap".into()],
        allowed_tools_added: vec!["Bash(v1:*)".into()],
        system_prompt_fragment: Some("v1 fragment".into()),
    };
    let v2 = ToolPack {
        name: pack_name.clone(),
        version: 2,
        binaries: vec!["v2-bin".into()],
        container_bootstrap: vec!["v2-bootstrap".into()],
        allowed_tools_added: vec!["Bash(v2:*)".into()],
        system_prompt_fragment: Some("v2 fragment".into()),
    };
    redis_io::seed_pack(&mut conn, &v1).await.expect("seed v1");
    redis_io::seed_pack(&mut conn, &v2).await.expect("seed v2");

    let role = minimal_role(vec![pack_name.clone()]);
    let resolved = resolve_role_with_packs(&role, &mut conn)
        .await
        .expect("resolve");

    assert!(
        resolved.binaries.contains(&"v2-bin".to_string()),
        "resolved role must include v2 (highest version) binaries, got {:?}",
        resolved.binaries,
    );
    assert!(
        !resolved.binaries.contains(&"v1-bin".to_string()),
        "resolved role must NOT include v1 binaries when v2 is seeded; got {:?}",
        resolved.binaries,
    );
    assert!(
        resolved
            .entrypoint_script
            .starts_with("v2-bootstrap\necho body"),
        "entrypoint must reflect v2's container_bootstrap (latest); got {:?}",
        resolved.entrypoint_script,
    );
    assert_eq!(
        resolved.system_prompt.as_deref(),
        Some("base prompt\n\nv2 fragment"),
        "system_prompt must reflect v2's fragment (latest)",
    );
    assert_eq!(
        resolved.allowed_tools.as_ref().map(|a| a.0.as_slice()),
        Some(&["Bash(v2:*)".to_string()][..]),
        "allowed_tools must come from v2 only",
    );

    use redis::AsyncCommands;
    let _: () = conn
        .del::<_, ()>(format!("agentry:tool_pack:{pack_name}:v1"))
        .await
        .expect("cleanup v1");
    let _: () = conn
        .del::<_, ()>(format!("agentry:tool_pack:{pack_name}:v2"))
        .await
        .expect("cleanup v2");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn resolve_role_with_packs_missing_pack_errors() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");

    let role = minimal_role(vec!["zz-no-such-pack".into()]);
    let res = resolve_role_with_packs(&role, &mut conn).await;
    assert!(
        res.is_err(),
        "missing pack reference must surface as an error (daemon misconfig); got {res:?}",
    );
}
