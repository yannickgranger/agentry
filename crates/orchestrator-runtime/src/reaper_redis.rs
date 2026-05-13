//! Redis adapter implementations for the reaper ports. See
//! [`crate::reaper_ports`] for the trait surface; this module holds
//! the production [`RedisInventory`] / [`RedisReaperSink`] and
//! their `redis::*` dependencies.

use crate::lifecycle_ports::EventSourceError;
use crate::reaper_ports::{BriefInventory, ReaperSink, REAPER_AGENT_ID};
use async_trait::async_trait;
use orchestrator_types::lifecycle::{BriefEvent, BriefStateRecord};
use orchestrator_types::{BriefId, Event, EventKind, Ts};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::Value as JsonValue;

/// Production [`BriefInventory`] adapter. SCAN-pages
/// `agentry:brief:*:state`, GETs each match, and parses the JSON
/// `BriefStateRecord`. Body lookups GET `agentry:brief:{id}:body`
/// and walk the JSON to `payload.budget.max_wall_seconds` — bypassing
/// `serde_json::from_str::<Brief>` keeps the reaper insensitive to
/// future Brief-shape changes outside the budget field.
pub struct RedisInventory {
    conn: ConnectionManager,
}

impl RedisInventory {
    #[must_use]
    pub fn new(conn: ConnectionManager) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl BriefInventory for RedisInventory {
    async fn list_state_records(&mut self) -> Result<Vec<BriefStateRecord>, EventSourceError> {
        let mut keys: Vec<String> = Vec::new();
        let mut cursor: u64 = 0;
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg("agentry:brief:*:state")
                .arg("COUNT")
                .arg(100)
                .query_async(&mut self.conn)
                .await
                .map_err(|e| EventSourceError::Backend {
                    detail: e.to_string(),
                })?;
            for key in batch {
                if key.ends_with(":state") && !key.ends_with(":state_log") {
                    keys.push(key);
                }
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let raw: Option<String> =
                self.conn
                    .get(&key)
                    .await
                    .map_err(|e| EventSourceError::Backend {
                        detail: e.to_string(),
                    })?;
            let Some(s) = raw else { continue };
            match serde_json::from_str::<BriefStateRecord>(&s) {
                Ok(r) => out.push(r),
                Err(e) => {
                    tracing::warn!(key = %key, error = %e, "reaper: skip malformed state record");
                }
            }
        }
        Ok(out)
    }

    async fn read_max_wall_seconds(
        &mut self,
        brief_id: &BriefId,
    ) -> Result<Option<u64>, EventSourceError> {
        let key = format!("agentry:brief:{}:body", brief_id.0);
        let raw: Option<String> =
            self.conn
                .get(&key)
                .await
                .map_err(|e| EventSourceError::Backend {
                    detail: e.to_string(),
                })?;
        let Some(raw) = raw else { return Ok(None) };
        let value: JsonValue = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(brief = %brief_id.0, error = %e, "reaper: malformed body json");
                return Ok(None);
            }
        };
        Ok(value
            .get("budget")
            .and_then(|b| b.get("max_wall_seconds"))
            .and_then(JsonValue::as_u64))
    }

    async fn last_trace_event_age(
        &mut self,
        brief_id: &BriefId,
        now: Ts,
    ) -> Result<Option<u64>, EventSourceError> {
        let stream = format!("agentry:brief:{}:trace", brief_id.0);
        let reply: redis::Value = match redis::cmd("XINFO")
            .arg("STREAM")
            .arg(&stream)
            .query_async(&mut self.conn)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                // Stream missing → ERR no such key; treat as not-orphan.
                if e.kind() == redis::ErrorKind::ResponseError
                    && e.to_string().to_lowercase().contains("no such key")
                {
                    return Ok(None);
                }
                return Err(EventSourceError::Backend {
                    detail: e.to_string(),
                });
            }
        };
        let mut length: Option<i64> = None;
        let mut last_id: Option<String> = None;
        if let redis::Value::Array(items) = reply {
            let mut iter = items.into_iter();
            while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
                let key = match k {
                    redis::Value::BulkString(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    redis::Value::SimpleString(s) => s,
                    _ => continue,
                };
                match key.as_str() {
                    "length" => {
                        if let redis::Value::Int(n) = v {
                            length = Some(n);
                        }
                    }
                    "last-generated-id" => match v {
                        redis::Value::BulkString(bytes) => {
                            last_id = Some(String::from_utf8_lossy(&bytes).into_owned());
                        }
                        redis::Value::SimpleString(s) => last_id = Some(s),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        if matches!(length, Some(0)) {
            return Ok(None);
        }
        let Some(id) = last_id else { return Ok(None) };
        let Some((ms_str, _)) = id.split_once('-') else {
            tracing::warn!(brief = %brief_id.0, last_id = %id, "reaper: malformed XID");
            return Ok(None);
        };
        let Ok(ms) = ms_str.parse::<i64>() else {
            tracing::warn!(brief = %brief_id.0, last_id = %id, "reaper: unparsable XID ms prefix");
            return Ok(None);
        };
        if ms == 0 {
            return Ok(None);
        }
        let last_secs = ms / 1000;
        let now_secs = now.timestamp();
        let delta = now_secs - last_secs;
        if delta < 0 {
            return Ok(None);
        }
        Ok(Some(delta as u64))
    }
}

/// Production [`ReaperSink`] adapter. Pushes lifecycle events to the
/// trace stream as `EventKind::Event { payload: <BriefEvent JSON> }`
/// — the [`crate::lifecycle::RedisEventSource`] translator
/// recognises this shape and yields the carried [`BriefEvent`] to the
/// per-brief FSM driver. `podman kill` shells out via
/// `tokio::process::Command`.
pub struct RedisReaperSink {
    conn: ConnectionManager,
}

impl RedisReaperSink {
    #[must_use]
    pub fn new(conn: ConnectionManager) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl ReaperSink for RedisReaperSink {
    async fn push_event(
        &mut self,
        brief_id: &BriefId,
        event: &BriefEvent,
    ) -> Result<(), EventSourceError> {
        let payload = serde_json::to_value(event).map_err(|e| EventSourceError::Parse {
            detail: format!("reaper push serialize: {e}"),
        })?;
        let trace_event = Event::new(EventKind::Event { payload });
        let body = serde_json::to_string(&trace_event).map_err(|e| EventSourceError::Parse {
            detail: format!("reaper push wrap: {e}"),
        })?;
        let stream = format!("agentry:brief:{}:trace", brief_id.0);
        let _: String = self
            .conn
            .xadd(
                &stream,
                "*",
                &[("agent", REAPER_AGENT_ID), ("event", body.as_str())],
            )
            .await
            .map_err(|e| EventSourceError::Backend {
                detail: e.to_string(),
            })?;
        Ok(())
    }

    async fn kill_containers(&mut self, brief_id: &BriefId) {
        let label_filter = format!("label=agentry.brief={}", brief_id.0);
        let output = tokio::process::Command::new("podman")
            .args([
                "ps",
                "--filter",
                label_filter.as_str(),
                "--format",
                "{{.Names}}",
            ])
            .output()
            .await;
        let names = match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            Ok(o) => {
                tracing::warn!(
                    brief = %brief_id.0,
                    status = ?o.status,
                    stderr = %String::from_utf8_lossy(&o.stderr),
                    "podman ps failed"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(brief = %brief_id.0, error = %e, "podman ps spawn failed");
                return;
            }
        };
        for name in names.split_whitespace() {
            match tokio::process::Command::new("podman")
                .args(["kill", name])
                .output()
                .await
            {
                Ok(o) if o.status.success() => {
                    tracing::info!(
                        brief = %brief_id.0,
                        container = %name,
                        "reaper killed orphan container"
                    );
                }
                Ok(o) => {
                    tracing::warn!(
                        brief = %brief_id.0,
                        container = %name,
                        status = ?o.status,
                        "podman kill failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        brief = %brief_id.0,
                        container = %name,
                        error = %e,
                        "podman kill spawn failed"
                    );
                }
            }
        }
    }
}
