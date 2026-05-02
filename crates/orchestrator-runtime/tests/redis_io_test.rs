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
    append_verdict_idempotent, connect, fetch_role, list_roles, list_teams, register_team_strict,
    save_role, RegisterOutcome, STREAM_VERDICTS,
};
use orchestrator_runtime::Error;
use orchestrator_types::{
    AgentRole, BriefId, PackageManager, PermitScope, RoleName, RoleRef, SubstrateClass, TeamName,
    TeamTopology, ToolAllowlist, Verdict, VerdictKind,
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

fn brief_slug(prefix: &str) -> String {
    format!(
        "brf_test_{prefix}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn shipped(brief_id: &str) -> Verdict {
    Verdict::new(BriefId(brief_id.into()), VerdictKind::Shipped)
}

/// First call against a fresh brief_id wins the SETNX, returns
/// `Ok(Some(stream_id))`, and lands the verdict on `agentry:verdicts`.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn append_verdict_idempotent_first_call_wins() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let brief_id = brief_slug("idem_first");
    let v = shipped(&brief_id);
    let sentinel_key = format!("agentry:verdict:emitted:{brief_id}");

    let before: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen before");
    let outcome = append_verdict_idempotent(&mut conn, &v)
        .await
        .expect("first call");
    let after: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen after");

    assert!(outcome.is_some(), "first call must claim and XADD");
    assert_eq!(after, before + 1, "verdict must be on stream");

    let _: () = conn.del(&sentinel_key).await.expect("cleanup sentinel");
}

/// Second call with the same brief_id is suppressed: returns
/// `Ok(None)` and the stream length stays put.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn append_verdict_idempotent_second_call_suppressed() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let brief_id = brief_slug("idem_second");
    let v = shipped(&brief_id);
    let sentinel_key = format!("agentry:verdict:emitted:{brief_id}");

    let before: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen before");
    let first = append_verdict_idempotent(&mut conn, &v)
        .await
        .expect("first call");
    let after_first: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen first");
    let second = append_verdict_idempotent(&mut conn, &v)
        .await
        .expect("second call");
    let after_second: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen second");

    assert!(first.is_some(), "first call wins");
    assert!(second.is_none(), "second call must be suppressed");
    assert_eq!(after_first, before + 1, "first call increments XLEN by 1");
    assert_eq!(
        after_second, after_first,
        "second call must NOT increment XLEN"
    );

    let _: () = conn.del(&sentinel_key).await.expect("cleanup sentinel");
}

/// Lock-down for the #178 success/role-outcome reproducer: a brief with
/// multiple roles completes, each role producing its own `Verdict` whose
/// `brief.0` carries the same brief_id. Without the gate, every per-role
/// `append_verdict` call XADDs onto `agentry:verdicts`, so the operator
/// sees N entries for one brief. The gate must collapse the sequence to a
/// single XADD: the first arriving role wins, the rest report `Ok(None)`
/// and the kind reported on stream is the first arrival's kind.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn append_verdict_idempotent_role_outcome_path_collapses_to_one() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let brief_id = brief_slug("idem_role_outcomes");
    let sentinel_key = format!("agentry:verdict:emitted:{brief_id}");

    // Three role outcomes for the same brief, with distinct kinds —
    // mirrors the loop at daemon.rs handle_brief role-outcome path.
    let role_a = Verdict::new(BriefId(brief_id.clone()), VerdictKind::Shipped);
    let role_b = Verdict::new(
        BriefId(brief_id.clone()),
        VerdictKind::ReworkNeeded { findings: vec![] },
    );
    let role_c = Verdict::new(BriefId(brief_id.clone()), VerdictKind::Failed);

    let before: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen before");
    let r_a = append_verdict_idempotent(&mut conn, &role_a)
        .await
        .expect("role a");
    let r_b = append_verdict_idempotent(&mut conn, &role_b)
        .await
        .expect("role b");
    let r_c = append_verdict_idempotent(&mut conn, &role_c)
        .await
        .expect("role c");
    let after: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen after");

    assert!(r_a.is_some(), "first role outcome must claim the sentinel");
    assert!(
        r_b.is_none(),
        "second role outcome on same brief must be suppressed"
    );
    assert!(
        r_c.is_none(),
        "third role outcome on same brief must be suppressed"
    );
    assert_eq!(
        after,
        before + 1,
        "exactly one verdict must land on stream regardless of role count"
    );

    let _: () = conn.del(&sentinel_key).await.expect("cleanup sentinel");
}

/// Distinct brief_ids each get their own sentinel; both XADD.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn append_verdict_idempotent_distinct_briefs() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let id_a = brief_slug("idem_dist_a");
    let id_b = brief_slug("idem_dist_b");
    let sentinel_a = format!("agentry:verdict:emitted:{id_a}");
    let sentinel_b = format!("agentry:verdict:emitted:{id_b}");

    let before: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen before");
    let r_a = append_verdict_idempotent(&mut conn, &shipped(&id_a))
        .await
        .expect("call a");
    let r_b = append_verdict_idempotent(&mut conn, &shipped(&id_b))
        .await
        .expect("call b");
    let after: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen after");

    assert!(r_a.is_some(), "brief A must XADD");
    assert!(r_b.is_some(), "brief B must XADD");
    assert_eq!(after, before + 2, "two verdicts must be on stream");

    let _: () = conn.del(&sentinel_a).await.expect("cleanup a");
    let _: () = conn.del(&sentinel_b).await.expect("cleanup b");
}
