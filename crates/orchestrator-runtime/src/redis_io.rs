//! Redis operations: brief submission, stream reads, verdict appends, trace writes.

use crate::{Error, Result};
use orchestrator_types::{
    AgentRole, Brief, BriefId, Event, RoleName, TeamName, TeamTopology, Verdict, VersionedRef,
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
pub async fn submit_brief(conn: &mut ConnectionManager, brief: &Brief) -> Result<String> {
    let body = serde_json::to_string(brief)?;
    let id: String = conn
        .xadd(STREAM_BRIEFS, "*", &[("brief", body.as_str())])
        .await?;
    Ok(id)
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
