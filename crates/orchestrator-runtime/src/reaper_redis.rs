//! Redis adapter implementations for the reaper ports. See
//! [`crate::reaper_ports`] for the trait surface; this module holds
//! the production [`RedisInventory`] / [`RedisReaperSink`] and
//! their `redis::*` dependencies.

use crate::lifecycle_ports::EventSourceError;
use crate::reaper_ports::{BriefInventory, ReaperSink, REAPER_AGENT_ID};
use async_trait::async_trait;
use orchestrator_types::lifecycle::{BriefEvent, BriefStateRecord};
use orchestrator_types::{BriefId, Event, EventKind};
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
