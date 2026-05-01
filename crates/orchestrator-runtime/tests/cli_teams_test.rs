//! Integration tests for `orchestrator team` CLI handlers.

use orchestrator_runtime::{
    cli_teams::{register, validate},
    redis_io,
};
use orchestrator_types::{MessageEdge, RoleName, RoleRef, TeamName, TeamTopology};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::io::Write;

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn slug() -> String {
    format!(
        "cli_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn write_topology(t: &TeamTopology) -> tempfile::NamedTempFile {
    let body = serde_json::to_string_pretty(t).expect("serialize topology");
    let mut f = tempfile::NamedTempFile::new().expect("tmp");
    f.write_all(body.as_bytes()).expect("write tmp");
    f
}

fn rn(s: &str) -> RoleName {
    RoleName(s.into())
}

fn rr(s: &str) -> RoleRef {
    RoleRef {
        name: rn(s),
        version: 1,
    }
}

fn topo(name: &str, version: u32, role: &str) -> TeamTopology {
    TeamTopology {
        name: TeamName(name.into()),
        version,
        roles: vec![rr(role)],
        message_graph: vec![],
        terminal_role: rr(role),
        max_retries: 0,
    }
}

async fn seed_role_v1(conn: &mut ConnectionManager, name: &str) {
    let body = serde_json::json!({"_": "probe"}).to_string();
    let key = format!("agentry:role:{name}:v1");
    let _: () = conn.set(&key, body.as_str()).await.expect("set role");
}

async fn cleanup_role(conn: &mut ConnectionManager, name: &str) {
    let key = format!("agentry:role:{name}:v1");
    let _: () = conn.del(&key).await.expect("cleanup role");
}

async fn cleanup_team(conn: &mut ConnectionManager, name: &str, version: u32) {
    let key = format!("agentry:team:{name}:v{version}");
    let _: () = conn.del(&key).await.expect("cleanup team");
}

#[test]
fn schema_outputs_valid_json() {
    let schema = schemars::schema_for!(TeamTopology);
    let v = serde_json::to_value(&schema).expect("schema must serialize as JSON");
    let obj = v.as_object().expect("schema root must be a JSON object");
    assert!(!obj.is_empty(), "schema root object must not be empty");
    assert!(
        obj.contains_key("properties"),
        "TeamTopology schema must expose a `properties` map (got keys: {:?})",
        obj.keys().collect::<Vec<_>>()
    );
}

#[test]
fn schema_includes_terminal_role_field() {
    let schema = schemars::schema_for!(TeamTopology);
    let v = serde_json::to_value(&schema).expect("schema must serialize as JSON");
    let props = v
        .get("properties")
        .and_then(|p| p.as_object())
        .expect("TeamTopology schema must have a `properties` object");
    assert!(
        props.contains_key("terminal_role"),
        "TeamTopology schema must include `terminal_role` (got: {:?})",
        props.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn register_rejects_unknown_role() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");
    let s = slug();
    let team_name = format!("zz-cli-{s}");
    let t = topo(&team_name, 1, &format!("zz-missing-{s}"));
    let f = write_topology(&t);

    let r = register(&mut conn, f.path()).await;
    assert!(r.is_err(), "register must error on validation failure");
    let key = format!("agentry:team:{team_name}:v1");
    let raw: Option<String> = conn.get(&key).await.expect("get");
    assert!(raw.is_none(), "register must not persist on violations");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn validate_rejects_cycle() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");
    let s = slug();
    let role_a = format!("zz-cli-a-{s}");
    let role_b = format!("zz-cli-b-{s}");
    seed_role_v1(&mut conn, &role_a).await;
    seed_role_v1(&mut conn, &role_b).await;

    let team_name = format!("zz-cli-cyc-{s}");
    let t = TeamTopology {
        name: TeamName(team_name.clone()),
        version: 1,
        roles: vec![rr(&role_a), rr(&role_b)],
        message_graph: vec![
            MessageEdge {
                from: rr(&role_a),
                to: rr(&role_b),
                permit_overrides_from: None,
                rework_target: None,
            },
            MessageEdge {
                from: rr(&role_b),
                to: rr(&role_a),
                permit_overrides_from: None,
                rework_target: None,
            },
        ],
        terminal_role: rr(&role_b),
        max_retries: 0,
    };
    let f = write_topology(&t);
    validate(&mut conn, f.path()).await.expect("validate runs");

    let key = format!("agentry:team:{team_name}:v1");
    let raw: Option<String> = conn.get(&key).await.expect("get");
    assert!(raw.is_none(), "validate must never persist");

    cleanup_role(&mut conn, &role_a).await;
    cleanup_role(&mut conn, &role_b).await;
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn register_rejects_duplicate_at_same_version() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");
    let s = slug();
    let role = format!("zz-cli-dup-{s}");
    seed_role_v1(&mut conn, &role).await;

    let team_name = format!("zz-cli-dup-team-{s}");
    let t = topo(&team_name, 1, &role);
    let f = write_topology(&t);

    register(&mut conn, f.path()).await.expect("first register");
    let again = register(&mut conn, f.path()).await;
    assert!(
        again.is_err(),
        "second register at same (name, version) must error"
    );

    cleanup_team(&mut conn, &team_name, 1).await;
    cleanup_role(&mut conn, &role).await;
}
