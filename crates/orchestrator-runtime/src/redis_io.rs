//! Redis operations: brief submission, stream reads, verdict appends, trace writes.

use crate::{Error, Result};
use orchestrator_types::{
    AgentRole, Brief, BriefId, Event, Project, RoleName, TeamName, TeamTopology, Verdict,
    VersionedRef,
};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

/// Stream names.
pub const STREAM_BRIEFS: &str = "agentry:briefs";
pub const STREAM_VERDICTS: &str = "agentry:verdicts";

/// Open a Redis connection manager from a pre-resolved URL.
/// URL is typically `config.redis.url` (loaded via figment).
pub async fn connect(url: &str) -> Result<ConnectionManager> {
    let client = redis::Client::open(url)?;
    let conn = ConnectionManager::new(client).await?;
    Ok(conn)
}

/// Submit a brief to the `agentry:briefs` stream. Returns the Redis stream id.
///
/// As a side effect:
/// * stashes the full brief body at `agentry:brief:<id>:body` so the DOL
///   composer (see `daemon::compose_meta_verdict`) can replay it without
///   scanning the stream.
/// * if the brief carries `parent_brief = Some(meta_id)`, registers it in the
///   meta-brief's `agentry:brief:<meta_id>:children_pending` set BEFORE the
///   XADD so the daemon can never observe the child reaching terminal verdict
///   while the set is missing the entry.
pub async fn submit_brief(conn: &mut ConnectionManager, brief: &Brief) -> Result<String> {
    let body = serde_json::to_string(brief)?;

    let body_key = format!("agentry:brief:{}:body", brief.id.0);
    let _: () = conn.set(&body_key, body.as_str()).await?;

    if let Some(meta_id) = &brief.parent_brief {
        let pending_key = format!("agentry:brief:{}:children_pending", meta_id.0);
        let _: () = conn.sadd(&pending_key, brief.id.0.as_str()).await?;
    }

    let id: String = conn
        .xadd(STREAM_BRIEFS, "*", &[("brief", body.as_str())])
        .await?;
    Ok(id)
}

/// Fetch a previously-submitted brief by id. Reads the body stashed at
/// `agentry:brief:<id>:body` by `submit_brief`. Used by the DOL composer to
/// replay a meta-brief's payload (notably its `success_criteria`) when its
/// last child resolves.
pub async fn fetch_brief_body(conn: &mut ConnectionManager, brief_id: &str) -> Result<Brief> {
    let key = format!("agentry:brief:{brief_id}:body");
    let raw: Option<String> = conn.get(&key).await?;
    let raw = raw.ok_or_else(|| crate::Error::NotFound {
        kind: "brief",
        key: key.clone(),
    })?;
    Ok(serde_json::from_str(&raw)?)
}

/// Append an event to a brief's trace stream.
pub async fn append_trace(
    conn: &mut ConnectionManager,
    brief: &BriefId,
    agent_id: &str,
    event: &Event,
) -> Result<()> {
    let body = serde_json::to_string(event)?;
    let stream = format!("agentry:brief:{}:trace", brief.0);
    let _: String = conn
        .xadd(
            &stream,
            "*",
            &[("agent", agent_id), ("event", body.as_str())],
        )
        .await?;
    Ok(())
}

/// Append a verdict to the verdicts stream.
pub async fn append_verdict(conn: &mut ConnectionManager, v: &Verdict) -> Result<String> {
    let body = serde_json::to_string(v)?;
    let id: String = conn
        .xadd(STREAM_VERDICTS, "*", &[("verdict", body.as_str())])
        .await?;
    Ok(id)
}

/// Fetch an agent role by versioned ref.
pub async fn fetch_role(
    conn: &mut ConnectionManager,
    name: &RoleName,
    version: u32,
) -> Result<AgentRole> {
    let key = format!("agentry:role:{}:v{}", name.0, version);
    let raw: Option<String> = conn.get(&key).await?;
    let raw = raw.ok_or_else(|| Error::NotFound {
        kind: "role",
        key: key.clone(),
    })?;
    Ok(serde_json::from_str(&raw)?)
}

/// Fetch a project record by slug. Project records are keyed
/// `agentry:project:<slug>` and are not versioned.
pub async fn fetch_project(conn: &mut ConnectionManager, slug: &str) -> Result<Project> {
    let key = format!("agentry:project:{slug}");
    let raw: Option<String> = conn.get(&key).await?;
    let raw = raw.ok_or_else(|| Error::NotFound {
        kind: "project",
        key: key.clone(),
    })?;
    Ok(serde_json::from_str(&raw)?)
}

/// Fetch a team topology by versioned ref.
pub async fn fetch_team(conn: &mut ConnectionManager, r: &VersionedRef) -> Result<TeamTopology> {
    let key = r.redis_key("team");
    let raw: Option<String> = conn.get(&key).await?;
    let raw = raw.ok_or_else(|| Error::NotFound {
        kind: "team",
        key: key.clone(),
    })?;
    Ok(serde_json::from_str(&raw)?)
}

/// Save a role.
pub async fn save_role(conn: &mut ConnectionManager, r: &AgentRole) -> Result<()> {
    let key = format!("agentry:role:{}:v{}", r.name.0, r.version);
    let body = serde_json::to_string(r)?;
    let _: () = conn.set(&key, body).await?;
    Ok(())
}

/// Save a team.
pub async fn save_team(conn: &mut ConnectionManager, t: &TeamTopology) -> Result<()> {
    let key = format!("agentry:team:{}:v{}", t.name.0, t.version);
    let body = serde_json::to_string(t)?;
    let _: () = conn.set(&key, body).await?;
    Ok(())
}

/// Outcome of an atomic team register: a first-writer-wins write that does
/// NOT overwrite an existing key at the same `(name, version)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterOutcome {
    Registered,
    AlreadyExists,
}

/// Atomically register a team topology under `agentry:team:<name>:v<version>`
/// using `SET ... NX` semantics. Returns `Registered` if this call wrote the
/// key, `AlreadyExists` if the key was already present (the existing body is
/// untouched). Coexists with [`save_team`], which is overwriting and intended
/// for seed-time use.
pub async fn register_team_strict(
    conn: &mut ConnectionManager,
    t: &TeamTopology,
) -> Result<RegisterOutcome> {
    let key = format!("agentry:team:{}:v{}", t.name.0, t.version);
    let body = serde_json::to_string(t)?;
    let acquired: bool = redis::cmd("SET")
        .arg(&key)
        .arg(body)
        .arg("NX")
        .query_async(conn)
        .await?;
    if acquired {
        Ok(RegisterOutcome::Registered)
    } else {
        Ok(RegisterOutcome::AlreadyExists)
    }
}

/// Scan the team catalog and return every `(name, version)` pair currently
/// registered, sorted by name then version ascending.
pub async fn list_teams(conn: &mut ConnectionManager) -> Result<Vec<(TeamName, u32)>> {
    let mut out: Vec<(TeamName, u32)> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("agentry:team:*:v*")
            .arg("COUNT")
            .arg(100)
            .query_async(conn)
            .await?;
        for key in batch {
            if let Some((name, version)) = parse_versioned_key(&key, "team") {
                out.push((TeamName(name), version));
            }
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    out.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then_with(|| a.1.cmp(&b.1)));
    Ok(out)
}

/// Scan the role catalog and return every `(name, version)` pair currently
/// registered, sorted by name then version ascending. Distinct from
/// [`list_role_names`], which dedupes versions.
pub async fn list_roles(conn: &mut ConnectionManager) -> Result<Vec<(RoleName, u32)>> {
    let mut out: Vec<(RoleName, u32)> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("agentry:role:*:v*")
            .arg("COUNT")
            .arg(100)
            .query_async(conn)
            .await?;
        for key in batch {
            if let Some((name, version)) = parse_versioned_key(&key, "role") {
                out.push((RoleName(name), version));
            }
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    out.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then_with(|| a.1.cmp(&b.1)));
    Ok(out)
}

/// Scan the role catalog and return every distinct role name, regardless of
/// version. Sorted ascending.
pub async fn list_role_names(conn: &mut ConnectionManager) -> Result<Vec<RoleName>> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut cursor: u64 = 0;
    loop {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("agentry:role:*:v*")
            .arg("COUNT")
            .arg(100)
            .query_async(conn)
            .await?;
        for key in batch {
            if let Some((name, _version)) = parse_versioned_key(&key, "role") {
                seen.insert(name);
            }
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    Ok(seen.into_iter().map(RoleName).collect())
}

/// Parse `agentry:<kind>:<name>:v<version>`. Returns `None` if the key shape
/// does not match (extra colons in `<name>` are tolerated by treating the
/// final `:v<digits>` as the version suffix).
fn parse_versioned_key(key: &str, kind: &str) -> Option<(String, u32)> {
    let prefix = format!("agentry:{kind}:");
    let rest = key.strip_prefix(&prefix)?;
    let (name, vsuffix) = rest.rsplit_once(":v")?;
    let version: u32 = vsuffix.parse().ok()?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), version))
}

/// Block-read the next brief from `agentry:briefs`, starting after `last_id`.
/// Returns `(stream_id, brief)`.
pub async fn read_next_brief(
    conn: &mut ConnectionManager,
    last_id: &str,
    block_ms: u64,
) -> Result<Option<(String, Brief)>> {
    let opts = redis::streams::StreamReadOptions::default()
        .block(usize::try_from(block_ms).unwrap_or(usize::MAX))
        .count(1);
    let reply: Option<redis::streams::StreamReadReply> = conn
        .xread_options(&[STREAM_BRIEFS], &[last_id], &opts)
        .await?;

    let Some(r) = reply else {
        return Ok(None);
    };

    for k in r.keys {
        for entry in k.ids {
            let sid = entry.id;
            let body: Option<String> = entry.map.get("brief").and_then(|v| match v {
                redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
                redis::Value::SimpleString(s) => Some(s.clone()),
                _ => None,
            });
            if let Some(b) = body {
                let brief: Brief = serde_json::from_str(&b)?;
                return Ok(Some((sid, brief)));
            }
        }
    }
    Ok(None)
}

/// Hint that we're skipping fields (silences dead-code warnings on unused helper types).
#[allow(dead_code)]
fn _unused(_: TeamName) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_versioned_key_basic() {
        assert_eq!(
            parse_versioned_key("agentry:team:foo:v1", "team"),
            Some(("foo".to_string(), 1))
        );
        assert_eq!(
            parse_versioned_key("agentry:role:coder-rust:v42", "role"),
            Some(("coder-rust".to_string(), 42))
        );
        // Wrong kind.
        assert_eq!(parse_versioned_key("agentry:team:foo:v1", "role"), None);
        // Missing version suffix.
        assert_eq!(parse_versioned_key("agentry:team:foo", "team"), None);
        // Non-numeric version tail.
        assert_eq!(parse_versioned_key("agentry:team:foo:vN", "team"), None);
    }

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
        TeamTopology {
            name: TeamName(name.into()),
            version,
            roles: vec![RoleName(role.into())],
            message_graph: vec![],
            terminal_role: RoleName(role.into()),
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

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn list_role_names_dedupes_versions() {
        let Some(url) = test_redis_url() else { return };
        let mut conn = connect(&url).await.expect("connect");
        let s = slug();
        let role_name = format!("zz-rio-role-{s}");
        // Seed two versions of the same role name. We construct the keys
        // directly to avoid pulling AgentRole's full shape into this test.
        let body = serde_json::json!({"_": "probe"}).to_string();
        let k1 = format!("agentry:role:{role_name}:v1");
        let k2 = format!("agentry:role:{role_name}:v2");
        let _: () = conn.set(&k1, body.as_str()).await.expect("set v1");
        let _: () = conn.set(&k2, body.as_str()).await.expect("set v2");

        let listed = list_role_names(&mut conn).await.expect("list");
        let count = listed.iter().filter(|r| r.0 == role_name).count();
        assert_eq!(
            count, 1,
            "list must dedupe across versions of the same role"
        );

        let _: () = conn.del(&k1).await.expect("cleanup v1");
        let _: () = conn.del(&k2).await.expect("cleanup v2");
    }
}
