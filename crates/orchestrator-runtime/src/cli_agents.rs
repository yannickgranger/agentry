//! Agent-fleet inspection helpers backing `orchestrator agents` CLI subcommands.
//! Pure functions over `State` + Redis ConnectionManager — the binary glues clap to these.

use crate::state::State;
use crate::Result;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::Value as JsonValue;

const STREAMS_SET_KEY: &str = "agentry:projector:streams";

/// SQL filter: `status='running'` unless `all` is true.
pub async fn list(state: &State, all: bool) -> Result<Vec<JsonValue>> {
    let sql = if all {
        "SELECT agent_id, brief_id, role_name, project, started_at, last_event_at, status, verdict, exit_code FROM agents ORDER BY started_at DESC"
    } else {
        "SELECT agent_id, brief_id, role_name, project, started_at, last_event_at, status FROM agents WHERE status = 'running' ORDER BY started_at DESC"
    };
    let rows = state.query(sql).await?;
    Ok(rows
        .into_iter()
        .map(|m| serde_json::to_value(m).unwrap_or(JsonValue::Null))
        .collect())
}

/// Passthrough to State::query (SELECT/WITH guard rejects writes structurally).
pub async fn query(state: &State, sql: &str) -> Result<Vec<JsonValue>> {
    let rows = state.query(sql).await?;
    Ok(rows
        .into_iter()
        .map(|m| serde_json::to_value(m).unwrap_or(JsonValue::Null))
        .collect())
}

/// Recent trace events emitted under `agent_id`, oldest-first. Walks every
/// known brief stream from `agentry:projector:streams`, XREVRANGE each, filters
/// by `agent` field, and returns up to `last` matches across all streams.
pub async fn trace(
    conn: &mut ConnectionManager,
    agent_id: &str,
    last: usize,
) -> Result<Vec<JsonValue>> {
    let brief_ids: Vec<String> = conn.smembers(STREAMS_SET_KEY).await.unwrap_or_default();
    let mut acc: Vec<(String, String, String)> = Vec::new(); // (entry_id, brief_id, body)
    for bid in &brief_ids {
        let key = format!("agentry:brief:{bid}:trace");
        let reply: redis::streams::StreamRangeReply = redis::cmd("XREVRANGE")
            .arg(&key)
            .arg("+")
            .arg("-")
            .arg("COUNT")
            .arg(last)
            .query_async(conn)
            .await
            .map_err(crate::Error::from)?;
        for entry in reply.ids {
            let agent_field = entry.map.get("agent").and_then(redis_value_as_str);
            if agent_field.as_deref() != Some(agent_id) {
                continue;
            }
            let body = entry
                .map
                .get("event")
                .and_then(redis_value_as_str)
                .unwrap_or_default();
            acc.push((entry.id, bid.clone(), body));
        }
    }
    acc.sort_by(|a, b| a.0.cmp(&b.0));
    if acc.len() > last {
        let drop = acc.len() - last;
        acc.drain(0..drop);
    }
    Ok(acc
        .into_iter()
        .map(|(id, bid, body)| {
            serde_json::json!({
                "entry_id": id,
                "brief_id": bid,
                "event": serde_json::from_str::<JsonValue>(&body)
                    .unwrap_or(JsonValue::String(body)),
            })
        })
        .collect())
}

/// Recent Status events for an agent. Wraps `trace` and filters to events
/// whose body contains `"type":"status"` (the EventKind serde tag).
pub async fn recent_status(
    conn: &mut ConnectionManager,
    agent_id: &str,
    count: usize,
) -> Result<Vec<JsonValue>> {
    let all = trace(conn, agent_id, count.saturating_mul(8)).await?;
    let filtered: Vec<JsonValue> = all
        .into_iter()
        .filter(|v| {
            v.get("event")
                .and_then(|ev| ev.get("type"))
                .and_then(JsonValue::as_str)
                == Some("status")
        })
        .collect();
    let drop = filtered.len().saturating_sub(count);
    Ok(filtered.into_iter().skip(drop).collect())
}

fn redis_value_as_str(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
        redis::Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_returns_running_only_by_default() {
        use crate::state;
        use chrono::Utc;
        let dir = tempfile::tempdir().expect("tmp");
        let s = state::open_or_init(&dir.path().join("state.db")).expect("open");
        let now = Utc::now();
        let mk = |id: &str, status: &str| state::AgentRow {
            agent_id: id.into(),
            brief_id: "brf_x".into(),
            role_name: "coder".into(),
            project: None,
            started_at: now,
            last_event_at: now,
            status: status.into(),
            verdict: None,
            exit_code: None,
            cohort_labels: vec![],
        };
        s.upsert_agent(&mk("agt_a", "running"))
            .await
            .expect("upsert a");
        s.upsert_agent(&mk("agt_b", "terminated"))
            .await
            .expect("upsert b");
        let only_running = list(&s, false).await.expect("list running");
        assert_eq!(only_running.len(), 1);
        let everyone = list(&s, true).await.expect("list all");
        assert_eq!(everyone.len(), 2);
    }

    #[tokio::test]
    async fn query_rejects_writes() {
        let dir = tempfile::tempdir().expect("tmp");
        let s = crate::state::open_or_init(&dir.path().join("state.db")).expect("open");
        assert!(query(&s, "DELETE FROM agents").await.is_err());
    }
}
