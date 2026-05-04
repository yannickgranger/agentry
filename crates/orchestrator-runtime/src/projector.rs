//! Projector — long-running task that materializes brief-trace events into
//! the agent-state SQLite store. Reads every known brief trace stream
//! (`agentry:brief:*:trace`) and projects `agent_event` payloads into the
//! `agents` + `cohort_labels` tables.
//!
//! Stream discovery: pattern XREAD isn't supported, so the spawner sadds the
//! brief id to `agentry:projector:streams` on first spawn-event. The projector
//! iterates that set every loop, so newly-spawned briefs are picked up on the
//! next poll cycle.
//!
//! Cursor durability: per-stream cursors persist in a Redis hash at
//! `agentry:projector:cursor` so the projector resumes mid-stream after a
//! restart instead of replaying or losing events.
//!
//! Failure model: any per-event failure (state-store error, malformed event,
//! non-utf8 bytes) is logged and skipped. The projector is a best-effort
//! shadow of the trace stream and must never tear down the daemon.

use crate::state::{AgentRow, State};
use chrono::{DateTime, Utc};
use orchestrator_types::{Event, EventKind};
use redis::aio::ConnectionManager;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;

const STREAMS_SET_KEY: &str = "agentry:projector:streams";
const CURSOR_HASH_KEY: &str = "agentry:projector:cursor";

/// Run the projector forever. Never returns under normal operation;
/// transient errors are logged and retried.
pub async fn run(state: Arc<State>, mut conn: ConnectionManager) -> ! {
    tracing::info!("projector: starting");
    loop {
        if let Err(e) = tick(&state, &mut conn).await {
            tracing::warn!(error = %e, "projector: tick failed");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn tick(state: &Arc<State>, conn: &mut ConnectionManager) -> redis::RedisResult<()> {
    let brief_ids: Vec<String> = conn.smembers(STREAMS_SET_KEY).await?;
    if brief_ids.is_empty() {
        tokio::time::sleep(Duration::from_millis(500)).await;
        return Ok(());
    }

    let streams: Vec<String> = brief_ids
        .iter()
        .map(|b| format!("agentry:brief:{b}:trace"))
        .collect();

    let mut cursors: Vec<String> = Vec::with_capacity(streams.len());
    for s in &streams {
        let cur: Option<String> = conn.hget(CURSOR_HASH_KEY, s).await?;
        cursors.push(cur.unwrap_or_else(|| "0-0".into()));
    }

    let opts = StreamReadOptions::default().block(2_000).count(64);
    let stream_refs: Vec<&str> = streams.iter().map(String::as_str).collect();
    let cursor_refs: Vec<&str> = cursors.iter().map(String::as_str).collect();

    let reply: Option<StreamReadReply> = conn
        .xread_options(&stream_refs, &cursor_refs, &opts)
        .await?;

    let Some(reply) = reply else {
        return Ok(());
    };

    for k in reply.keys {
        let stream_name = k.key.clone();
        let mut last_id_in_batch: Option<String> = None;
        for entry in k.ids {
            last_id_in_batch = Some(entry.id.clone());
            handle_entry(state, &entry).await;
        }
        if let Some(last) = last_id_in_batch {
            let _: () = conn.hset(CURSOR_HASH_KEY, &stream_name, last).await?;
        }
    }

    Ok(())
}

async fn handle_entry(state: &Arc<State>, entry: &redis::streams::StreamId) {
    let agent_id = match entry.map.get("agent").and_then(redis_value_as_str) {
        Some(s) => s,
        None => return,
    };
    let body = match entry.map.get("event").and_then(redis_value_as_str) {
        Some(b) => b,
        None => return,
    };
    let event: Event = match serde_json::from_str(&body) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "projector: malformed event");
            return;
        }
    };

    match &event.kind {
        EventKind::Event { payload } => {
            project_payload(state, &agent_id, &event.at, payload).await;
        }
        _ => {
            // All non-`event` event kinds advance the watermark only.
            if let Err(e) = state.update_last_event_at(&agent_id, event.at).await {
                tracing::warn!(error = %e, agent = %agent_id, "projector: update_last_event_at failed");
            }
        }
    }
}

async fn project_payload(
    state: &Arc<State>,
    agent_id: &str,
    event_at: &DateTime<Utc>,
    payload: &JsonValue,
) {
    let agent_event = payload.get("agent_event").and_then(JsonValue::as_str);

    match agent_event {
        Some("spawned") => {
            let row = match build_agent_row(agent_id, event_at, payload) {
                Some(r) => r,
                None => {
                    tracing::warn!(agent = %agent_id, "projector: spawn payload missing required fields");
                    return;
                }
            };
            let labels = row.cohort_labels.clone();
            if let Err(e) = state.upsert_agent(&row).await {
                tracing::warn!(error = %e, agent = %agent_id, "projector: upsert_agent failed");
                return;
            }
            for label in &labels {
                if let Err(e) = state.add_cohort_label(agent_id, label).await {
                    tracing::warn!(error = %e, agent = %agent_id, label = %label, "projector: add_cohort_label failed");
                }
            }
        }
        Some("terminated") => {
            let verdict = payload
                .get("verdict")
                .and_then(JsonValue::as_str)
                .unwrap_or("unknown");
            let exit_code = payload
                .get("exit_code")
                .and_then(JsonValue::as_i64)
                .and_then(|i| i32::try_from(i).ok());
            if let Err(e) = state.mark_terminated(agent_id, verdict, exit_code).await {
                tracing::warn!(error = %e, agent = %agent_id, "projector: mark_terminated failed");
            }
        }
        _ => {
            if let Err(e) = state.update_last_event_at(agent_id, *event_at).await {
                tracing::warn!(error = %e, agent = %agent_id, "projector: update_last_event_at failed");
            }
        }
    }
}

fn build_agent_row(
    agent_id: &str,
    event_at: &DateTime<Utc>,
    payload: &JsonValue,
) -> Option<AgentRow> {
    let brief_id = payload.get("brief_id").and_then(JsonValue::as_str)?;
    let role_name = payload.get("role_name").and_then(JsonValue::as_str)?;
    let project = payload
        .get("project")
        .and_then(|v| v.as_str().map(str::to_string));
    let started_at = payload
        .get("started_at")
        .and_then(JsonValue::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(*event_at);
    let cohort_labels: Vec<String> = payload
        .get("cohort_labels")
        .and_then(JsonValue::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Some(AgentRow {
        agent_id: agent_id.to_string(),
        brief_id: brief_id.to_string(),
        role_name: role_name.to_string(),
        project,
        started_at,
        last_event_at: *event_at,
        status: "running".into(),
        verdict: None,
        exit_code: None,
        cohort_labels,
    })
}

fn redis_value_as_str(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
        redis::Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}
