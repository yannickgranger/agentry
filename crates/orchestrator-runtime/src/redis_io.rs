//! Redis operations: brief submission, stream reads, verdict appends, trace writes.

use crate::{Error, Result};
use orchestrator_types::lifecycle::MAXIMUM_ATTEMPT_CAP;
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
///
/// Dispatch-time fence: rejects topologies whose `max_retries` exceeds
/// [`MAXIMUM_ATTEMPT_CAP`] BEFORE any Redis write, so an over-budget
/// topology never lands in the catalog.
pub async fn register_team_strict(
    conn: &mut ConnectionManager,
    t: &TeamTopology,
) -> Result<RegisterOutcome> {
    if t.max_retries > MAXIMUM_ATTEMPT_CAP {
        return Err(Error::Config(format!(
            "team {}:v{} declares max_retries={} which exceeds MAXIMUM_ATTEMPT_CAP={}",
            t.name.0, t.version, t.max_retries, MAXIMUM_ATTEMPT_CAP
        )));
    }
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
/// registered, sorted by name then version ascending.
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
