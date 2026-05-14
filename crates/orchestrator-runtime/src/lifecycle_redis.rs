//! Redis adapter implementations for the lifecycle ports. See
//! [`crate::lifecycle_ports`] for the trait surface; this module holds
//! the production [`RedisEventSource`] / [`RedisStateProjector`] and
//! their `redis::*` dependencies.

use crate::lifecycle_ports::{
    translate_trace_entry, EventSource, EventSourceError, StateProjector, StateProjectorError,
};
use async_trait::async_trait;
use orchestrator_infra::redis_io::redis_value_as_str;
use orchestrator_types::lifecycle::{BriefEvent, BriefStateRecord};
use orchestrator_types::{BriefId, Event};
use redis::aio::ConnectionManager;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use std::collections::HashMap;

/// Production [`EventSource`] adapter. Subscribes to
/// `agentry:brief:{id}:trace` via blocking `XREAD` and translates each
/// [`EventKind`]+role-name pair into the matching [`BriefEvent`].
///
/// The cursor starts at `"0-0"` per the spec invariant — the trace
/// stream is empty at brief dispatch and every event arrives after the
/// `XREAD` returns, so starting at `"0-0"` (rather than `"$"`) is
/// race-free.
pub struct RedisEventSource {
    conn: ConnectionManager,
    brief_id: BriefId,
    cursor: String,
    role_by_agent: HashMap<String, String>,
}

impl RedisEventSource {
    /// Construct a fresh source whose cursor begins at `"0-0"`.
    #[must_use]
    pub fn new(conn: ConnectionManager, brief_id: BriefId) -> Self {
        Self {
            conn,
            brief_id,
            cursor: "0-0".to_string(),
            role_by_agent: HashMap::new(),
        }
    }

    /// Construct from a persisted cursor — used on daemon restart so the
    /// projector resumes from the last entry it processed before crash.
    #[must_use]
    pub fn resume_from(conn: ConnectionManager, brief_id: BriefId, cursor: String) -> Self {
        Self {
            conn,
            brief_id,
            cursor,
            role_by_agent: HashMap::new(),
        }
    }
}

#[async_trait]
impl EventSource for RedisEventSource {
    async fn next(&mut self) -> Result<Option<BriefEvent>, EventSourceError> {
        let stream = format!("agentry:brief:{}:trace", self.brief_id.0);
        loop {
            let opts = StreamReadOptions::default().block(0).count(16);
            let reply: Option<StreamReadReply> = self
                .conn
                .xread_options(&[stream.as_str()], &[self.cursor.as_str()], &opts)
                .await
                .map_err(|e| EventSourceError::Backend {
                    detail: e.to_string(),
                })?;
            let Some(reply) = reply else {
                return Ok(None);
            };
            for k in reply.keys {
                for entry in k.ids {
                    self.cursor = entry.id.clone();
                    let agent_id = entry
                        .map
                        .get("agent")
                        .and_then(redis_value_as_str)
                        .ok_or_else(|| EventSourceError::Parse {
                            detail: format!("trace entry {} missing 'agent' field", entry.id),
                        })?;
                    let body = entry
                        .map
                        .get("event")
                        .and_then(redis_value_as_str)
                        .ok_or_else(|| EventSourceError::Parse {
                            detail: format!("trace entry {} missing 'event' field", entry.id),
                        })?;
                    let event: Event =
                        serde_json::from_str(&body).map_err(|e| EventSourceError::Parse {
                            detail: e.to_string(),
                        })?;
                    if let Some(brief_event) =
                        translate_trace_entry(&mut self.role_by_agent, agent_id, event)?
                    {
                        return Ok(Some(brief_event));
                    }
                }
            }
        }
    }
}

/// Production [`StateProjector`] adapter. Executes
/// [`LUA_PROJECTOR_WRITE`] for the atomic three-key write — state log
/// XADD, state key SET, cursor key SET — under a single
/// `EVALSHA`/`SCRIPT LOAD` round-trip per brief.
pub struct RedisStateProjector {
    conn: ConnectionManager,
    brief_id: BriefId,
    lua_sha: Option<String>,
}

/// Lua script body for the atomic projector write. Loaded into Redis via
/// `SCRIPT LOAD` on first use and invoked via `EVALSHA` thereafter.
///
/// `KEYS`:
///   1. `state_log` stream key (`agentry:brief:{id}:state_log`)
///   2. latest-state key (`agentry:brief:{id}:state`)
///   3. projector-cursor key (`agentry:brief:{id}:state_projector_cursor`)
///
/// `ARGV`:
///   1. JSON-serialised [`BriefStateRecord`]
///   2. trace-stream entry ID just consumed (the new cursor value)
///
/// Returns `1` on success. Aborts atomically on Redis OOM — the three
/// writes either all land or none do.
pub const LUA_PROJECTOR_WRITE: &str = r#"
redis.call('XADD', KEYS[1], '*', 'record', ARGV[1])
redis.call('SET', KEYS[2], ARGV[1])
redis.call('SET', KEYS[3], ARGV[2])
return 1
"#;

impl RedisStateProjector {
    /// Construct a projector that will lazy-load [`LUA_PROJECTOR_WRITE`]
    /// into Redis on first [`StateProjector::write`] call.
    #[must_use]
    pub fn new(conn: ConnectionManager, brief_id: BriefId) -> Self {
        Self {
            conn,
            brief_id,
            lua_sha: None,
        }
    }
}

#[async_trait]
impl StateProjector for RedisStateProjector {
    async fn write(
        &mut self,
        record: &BriefStateRecord,
        last_trace_id: &str,
    ) -> Result<(), StateProjectorError> {
        let json = serde_json::to_string(record).map_err(|e| StateProjectorError::LuaFailed {
            detail: e.to_string(),
        })?;
        let state_log_key = format!("agentry:brief:{}:state_log", self.brief_id.0);
        let state_key = format!("agentry:brief:{}:state", self.brief_id.0);
        let cursor_key = format!("agentry:brief:{}:state_projector_cursor", self.brief_id.0);

        let sha = match &self.lua_sha {
            Some(s) => s.clone(),
            None => {
                let loaded: String = redis::cmd("SCRIPT")
                    .arg("LOAD")
                    .arg(LUA_PROJECTOR_WRITE)
                    .query_async(&mut self.conn)
                    .await
                    .map_err(|e| StateProjectorError::Backend {
                        detail: e.to_string(),
                    })?;
                self.lua_sha = Some(loaded.clone());
                loaded
            }
        };

        let _: i64 = redis::cmd("EVALSHA")
            .arg(&sha)
            .arg(3)
            .arg(&state_log_key)
            .arg(&state_key)
            .arg(&cursor_key)
            .arg(json)
            .arg(last_trace_id)
            .query_async(&mut self.conn)
            .await
            .map_err(|e| StateProjectorError::Backend {
                detail: e.to_string(),
            })?;
        Ok(())
    }
}

/// Read the latest `BriefStateRecord` for a brief from Redis.
///
/// The FSM writes records through [`RedisStateProjector`] to the
/// `agentry:brief:{id}:state` key (atomic three-key write at
/// [`LUA_PROJECTOR_WRITE`]). This function is the symmetric reader —
/// returns `Ok(None)` when the key is missing (brief dispatched but no
/// state transition has fired yet, or the brief never existed), and
/// `Ok(Some(record))` when the projector has written.
///
/// Required by the v2-finale daemon-collapse work (#539): the team-
/// orchestration loop in `daemon::handle_brief` must observe FSM state
/// each iteration instead of carrying parallel in-process accumulators
/// (`shipped_roles`, `reworks_used`). The synthesis pure-projection
/// invariant — "a brief's progression at any moment is fully derivable
/// from `(BriefState, TeamTopology, trace stream)`" — depends on this
/// read path being available outside the daemon-resume boot scan.
///
/// Malformed JSON returns `Err(StateProjectorError::LuaFailed)` — the
/// error variant is reused for any payload-shape failure since this
/// signals projector-side corruption (the writer is the only producer
/// of this key).
pub async fn read_brief_state(
    conn: &mut ConnectionManager,
    brief_id: &BriefId,
) -> Result<Option<BriefStateRecord>, StateProjectorError> {
    let state_key = format!("agentry:brief:{}:state", brief_id.0);
    let raw: Option<String> = conn
        .get(&state_key)
        .await
        .map_err(|e| StateProjectorError::Backend {
            detail: e.to_string(),
        })?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let record: BriefStateRecord =
        serde_json::from_str(&raw).map_err(|e| StateProjectorError::LuaFailed {
            detail: format!("malformed state record at {state_key}: {e}"),
        })?;
    Ok(Some(record))
}
