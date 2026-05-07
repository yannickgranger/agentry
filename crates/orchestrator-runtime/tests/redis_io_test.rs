//! Integration tests for `redis_io` — public-surface helpers and their
//! Redis-gated semantics.
//!
//! Live-Redis tests gate on `AGENTRY_TEST_REDIS_URL` (default
//! `redis://127.0.0.1:6380`) and stay `#[ignore]` so the workspace-wide
//! `cargo test` pass stays green without a Redis dependency. The pure
//! `parse_versioned_key` test was dropped during the migration: it
//! exercised a private helper, and its behaviour is exercised end-to-end
//! by `list_teams_returns_sorted_pairs` and `list_roles_returns_sorted_pairs`.
//!
//! Run the live tests with:
//! `cargo test --package orchestrator-runtime --test redis_io_test -- --ignored`.

use orchestrator_runtime::redis_io::{
    connect, fetch_role, list_roles, list_teams, register_team_strict, save_role, RegisterOutcome,
};
use orchestrator_runtime::Error;
use orchestrator_types::{
    AgentRole, PackageManager, PermitScope, RoleName, RoleRef, SubstrateClass, TeamName,
    TeamTopology, ToolAllowlist,
};
use redis::AsyncCommands;

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn slug() -> String {
    format!(
        "rio_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn topo(name: &str, version: u32, role: &str) -> TeamTopology {
    let role_ref = RoleRef {
        name: RoleName(role.into()),
        version: 1,
    };
    TeamTopology {
        name: TeamName(name.into()),
        version,
        roles: vec![role_ref.clone()],
        message_graph: vec![],
        terminal_role: role_ref,
        max_retries: 0,
    }
}

/// Dispatch-time fence: a topology with `max_retries` above
/// `MAXIMUM_ATTEMPT_CAP` is rejected BEFORE any Redis write — the
/// validation runs at the top of `register_team_strict`, ahead of the
/// `SET ... NX` call. Live-Redis like the rest of this file: when
/// `AGENTRY_TEST_REDIS_URL` is unset the test no-ops (matches the file's
/// existing pattern); under CI the rejection is genuinely exercised.
#[tokio::test]
async fn register_team_strict_rejects_over_cap_max_retries() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let mut t = topo("zz-rio-cap-fence", 1, "echo-agent");
    t.max_retries = 999;

    let err = register_team_strict(&mut conn, &t)
        .await
        .expect_err("over-cap topology must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("999"),
        "rejection message must mention the offending value 999, got: {msg}"
    );
    assert!(
        msg.contains("MAXIMUM_ATTEMPT_CAP"),
        "rejection message must name MAXIMUM_ATTEMPT_CAP, got: {msg}"
    );

    let key = "agentry:team:zz-rio-cap-fence:v1";
    let raw: Option<String> = conn.get(key).await.expect("get");
    assert!(raw.is_none(), "rejected topology must not be persisted");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn register_team_strict_first_writer_wins() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let s = slug();
    let name = format!("team-{s}");
    let t1 = topo(&name, 1, "echo-agent");
    // Different body but same key.
    let mut t2 = t1.clone();
    t2.max_retries = 7;

    let r1 = register_team_strict(&mut conn, &t1).await.expect("first");
    assert_eq!(r1, RegisterOutcome::Registered);
    let r2 = register_team_strict(&mut conn, &t2).await.expect("second");
    assert_eq!(r2, RegisterOutcome::AlreadyExists);

    // Body must equal t1 (first writer's body), not t2.
    let key = format!("agentry:team:{name}:v1");
    let raw: Option<String> = conn.get(&key).await.expect("get");
    let raw = raw.expect("body present");
    let back: TeamTopology = serde_json::from_str(&raw).expect("parse");
    assert_eq!(back.max_retries, 0, "first writer's body must be retained");

    let _: () = conn.del(&key).await.expect("cleanup");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn list_teams_returns_sorted_pairs() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let s = slug();
    let n_a = format!("zz-{s}-alpha");
    let n_b = format!("zz-{s}-beta");
    let teams = vec![
        topo(&n_b, 2, "echo-agent"),
        topo(&n_a, 1, "echo-agent"),
        topo(&n_a, 2, "echo-agent"),
    ];
    for t in &teams {
        let _ = register_team_strict(&mut conn, t).await.expect("register");
    }
    let listed = list_teams(&mut conn).await.expect("list");

    let ours: Vec<(TeamName, u32)> = listed
        .into_iter()
        .filter(|(n, _)| n.0 == n_a || n.0 == n_b)
        .collect();
    assert_eq!(
        ours,
        vec![
            (TeamName(n_a.clone()), 1),
            (TeamName(n_a.clone()), 2),
            (TeamName(n_b.clone()), 2),
        ],
        "teams must come back sorted by name then version"
    );

    for (name, version) in [(n_a.clone(), 1), (n_a.clone(), 2), (n_b.clone(), 2)] {
        let key = format!("agentry:team:{name}:v{version}");
        let _: () = conn.del(&key).await.expect("cleanup");
    }
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn list_roles_returns_sorted_pairs() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let s = slug();
    let n_a = format!("zz-rio-list-a-{s}");
    let n_b = format!("zz-rio-list-b-{s}");
    // Seed two versions per role under our own slug — keys constructed
    // directly to avoid pulling AgentRole's full shape into this test.
    let body = serde_json::json!({"_": "probe"}).to_string();
    let keys = vec![
        format!("agentry:role:{n_b}:v2"),
        format!("agentry:role:{n_a}:v1"),
        format!("agentry:role:{n_a}:v2"),
        format!("agentry:role:{n_b}:v1"),
    ];
    for k in &keys {
        let _: () = conn.set(k, body.as_str()).await.expect("set");
    }

    let listed = list_roles(&mut conn).await.expect("list");
    let ours: Vec<(RoleName, u32)> = listed
        .into_iter()
        .filter(|(n, _)| n.0 == n_a || n.0 == n_b)
        .collect();
    assert_eq!(
        ours,
        vec![
            (RoleName(n_a.clone()), 1),
            (RoleName(n_a.clone()), 2),
            (RoleName(n_b.clone()), 1),
            (RoleName(n_b.clone()), 2),
        ],
        "roles must come back sorted by name then version"
    );

    for k in &keys {
        let _: () = conn.del(k).await.expect("cleanup");
    }
}

/// `fetch_role(name, version)` must return the EXACT version requested.
/// Pre-184b the daemon probed v1..v5 in order and returned the first hit —
/// so a topology pinning v2 still got v1 if v1 existed. The migration to
/// RoleRef-keyed lookups makes the version pin load-bearing; this test
/// pins that semantic.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn fetch_role_resolves_exact_version() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let s = slug();
    let role_name = RoleName(format!("zz-rio-fetch-{s}"));

    let v1 = AgentRole {
        name: role_name.clone(),
        version: 1,
        model: None,
        system_prompt: Some("v1 prompt".into()),
        image: "alpine:3.21".into(),
        substrate_class: SubstrateClass::default(),
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
        tool_packs: vec![],
    };
    let mut v2 = v1.clone();
    v2.version = 2;
    v2.system_prompt = Some("v2 prompt".into());

    save_role(&mut conn, &v1).await.expect("save v1");
    save_role(&mut conn, &v2).await.expect("save v2");

    let got_v2 = fetch_role(&mut conn, &role_name, 2)
        .await
        .expect("fetch v2");
    assert_eq!(got_v2.version, 2);
    assert_eq!(got_v2.system_prompt.as_deref(), Some("v2 prompt"));

    let got_v1 = fetch_role(&mut conn, &role_name, 1)
        .await
        .expect("fetch v1");
    assert_eq!(got_v1.version, 1);
    assert_eq!(got_v1.system_prompt.as_deref(), Some("v1 prompt"));

    // Asking for a version that wasn't seeded must NotFound — no
    // fall-through to a different version.
    let missing = fetch_role(&mut conn, &role_name, 7).await;
    assert!(matches!(missing, Err(Error::NotFound { .. })));

    let _: () = conn
        .del(format!("agentry:role:{}:v1", role_name.0))
        .await
        .expect("cleanup v1");
    let _: () = conn
        .del(format!("agentry:role:{}:v2", role_name.0))
        .await
        .expect("cleanup v2");
}
