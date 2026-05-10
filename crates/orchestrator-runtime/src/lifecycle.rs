//! Brief lifecycle Redis adapters — port traits + production Redis impls.
//!
//! Implements `specs/concepts/brief_state_stream.md` (this PR). Consumes
//! the FSM types from `orchestrator_types::lifecycle` (L.1, PR #304).
//!
//! Adapters ship UNWIRED in this slice — L.3a (#300) wires them into the
//! daemon's per-brief loop. The pub surface is two ports
//! ([`EventSource`], [`StateProjector`]) plus two Redis production
//! adapters ([`RedisEventSource`], [`RedisStateProjector`]) and two
//! error enums ([`EventSourceError`], [`StateProjectorError`]).

use async_trait::async_trait;
use orchestrator_types::lifecycle::{BriefEvent, BriefStateRecord, CiState};
use orchestrator_types::{BriefId, Event, EventKind, EventVerdict};
use redis::aio::ConnectionManager;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use thiserror::Error;

/// `DoneReason.cause` sentinel that the coder runner emits on the
/// terminal Shipped event when its acceptance check passed against an
/// empty diff (work was already on the base branch). The translator
/// folds this into a [`BriefEvent::CoderDoneNoOp`] so the FSM
/// short-circuits Authoring → Shipped instead of walking the full
/// Verifying / Reviewing / Shipping / Watching trail.
pub const NO_OP_SHORT_CIRCUIT_CAUSE: &str = "no_op_short_circuit";

/// Operator-visible reason text written to `agentry:verdicts` when the
/// FSM short-circuits to Shipped via [`NO_OP_SHORT_CIRCUIT_CAUSE`].
pub const NO_OP_VERDICT_REASON: &str = "no-op brief — coder produced no diff against base";

/// Errors surfaced by [`EventSource`] implementations. Production adapters
/// can hit [`Self::Backend`] on connection or read failures, and
/// [`Self::Parse`] when a trace stream entry does not deserialise into an
/// [`Event`] or carries the unexpected field shape.
#[derive(Debug, Error)]
pub enum EventSourceError {
    #[error("backend error: {detail}")]
    Backend { detail: String },
    #[error("trace event parse failed: {detail}")]
    Parse { detail: String },
}

/// Errors surfaced by [`StateProjector`] implementations. Production
/// adapters can hit [`Self::Backend`] on connection or write failures, and
/// [`Self::LuaFailed`] when the embedded Lua script body returns an error
/// reply (most commonly a Redis OOM abort, which is atomic per spec).
#[derive(Debug, Error)]
pub enum StateProjectorError {
    #[error("backend error: {detail}")]
    Backend { detail: String },
    #[error("lua script eval failed: {detail}")]
    LuaFailed { detail: String },
}

/// Port: yields [`BriefEvent`]s for a single brief. Production
/// implementations subscribe to the brief's trace stream
/// (`agentry:brief:{id}:trace`) and translate each agent-emitted
/// [`EventKind`] plus the agent's role name into the matching
/// [`BriefEvent`]. Tests pass an in-memory [`std::collections::VecDeque`]
/// of fixture events.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Returns the next [`BriefEvent`] for this brief, or `None` when no
    /// further events will arrive. Implementations are responsible for
    /// blocking the caller until an event is available.
    async fn next(&mut self) -> Result<Option<BriefEvent>, EventSourceError>;
}

/// Port: writes a [`BriefStateRecord`] to durable storage. The production
/// adapter executes a single Lua script that XADDs to the per-brief
/// state log, SETs the latest-state key, and SETs the projector cursor
/// atomically — Redis Lua aborts cleanly on OOM, so the three keys
/// either all advance or none do.
#[async_trait]
pub trait StateProjector: Send + Sync {
    /// Persist `record` and advance the projector cursor to
    /// `last_trace_id` (the trace-stream entry that produced `record`).
    async fn write(
        &mut self,
        record: &BriefStateRecord,
        last_trace_id: &str,
    ) -> Result<(), StateProjectorError>;
}

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

/// Translate one trace-stream `(agent_id, Event)` pair into the matching
/// `BriefEvent`, threading the per-source agent-id → role-kind memo.
/// Free function so unit tests can drive it without standing up a
/// `ConnectionManager`. Public so the peer `tests/lifecycle.rs`
/// integration suite can drive it directly with synthesised
/// `Event`s — the workspace's `arch-ban-inline-cfg-test-in-src.cypher`
/// rule forbids inline `#[cfg(test)]` blocks here.
pub fn translate_trace_entry(
    role_by_agent: &mut HashMap<String, String>,
    agent_id: String,
    event: Event,
) -> Result<Option<BriefEvent>, EventSourceError> {
    match event.kind {
        EventKind::Event { payload } => {
            if payload.get("agent_event").and_then(JsonValue::as_str) == Some("spawned") {
                if let Some(role) = payload.get("role_name").and_then(JsonValue::as_str) {
                    if let Some(kind) = orchestrator_types::lifecycle::role_kind(role) {
                        role_by_agent.insert(agent_id.clone(), role.to_string());
                        if kind == "coder" {
                            return Ok(Some(BriefEvent::CoderStarted { agent_id }));
                        }
                    }
                }
            }
            // Daemon-side lifecycle pushes (the wall-clock reaper, the
            // handler-error AbortRequested in `daemon::run`) wrap a
            // serialised `BriefEvent` as the `Event` payload. Detect
            // the BriefEvent shape via its serde tag and yield it
            // straight through so the FSM driver can apply it. The
            // `kind` field is the discriminator; we only attempt the
            // deserialise when it's present and string-typed to keep
            // the cost off the hot agent-event path.
            if payload.get("kind").and_then(JsonValue::as_str).is_some() {
                if let Ok(brief_event) = serde_json::from_value::<BriefEvent>(payload.clone()) {
                    return Ok(Some(brief_event));
                }
            }
            Ok(None)
        }
        EventKind::Done {
            verdict, reason, ..
        } => {
            let Some(role_name) = role_by_agent.get(&agent_id).cloned() else {
                return Ok(None);
            };
            match orchestrator_types::lifecycle::role_kind(&role_name) {
                Some("coder") => {
                    if verdict == EventVerdict::Shipped
                        && reason.as_ref().map(|r| r.cause.as_str())
                            == Some(NO_OP_SHORT_CIRCUIT_CAUSE)
                    {
                        return Ok(Some(BriefEvent::CoderDoneNoOp {
                            reason: NO_OP_VERDICT_REASON.to_string(),
                        }));
                    }
                    Ok(Some(BriefEvent::CoderDone { verdict }))
                }
                Some("ac-verifier") | Some("verifier") => {
                    Ok(Some(BriefEvent::AcVerifierDone { verdict, role_name }))
                }
                Some("reviewer") => Ok(Some(BriefEvent::ReviewerDone {
                    verdict,
                    findings: vec![],
                    role_name,
                })),
                // Shipper Done. Acceptance pr_number / head_sha ride
                // separately on the trace stream as the shipper's own
                // emit_message to ci-watcher; the FSM's
                // Shipping → Watching arm carries the values that
                // were already sealed into Shipping. Defaults are
                // safe here — the existing FSM arm only reads the
                // event's pr_number / head_sha, and the lifecycle
                // driver overrides them at write time.
                Some("shipper") => Ok(Some(BriefEvent::ShipperDone {
                    pr_number: 0,
                    head_sha: String::new(),
                })),
                // CI watcher Done. Map the agent verdict onto the
                // poller-shaped CiState the FSM expects:
                //   Shipped → Success, Failed → Failed, else Pending.
                // The else arm covers Escalated / ReworkNeeded /
                // Rejected, which the watcher does not normally emit
                // but which the FSM treats as no-op Pending in the
                // Watching state.
                Some("ci-watcher") => {
                    let state = match verdict {
                        EventVerdict::Shipped => CiState::Success,
                        EventVerdict::Failed => CiState::Failed,
                        EventVerdict::Escalated
                        | EventVerdict::ReworkNeeded
                        | EventVerdict::Rejected => CiState::Pending,
                    };
                    Ok(Some(BriefEvent::CiResult {
                        state,
                        head_sha: String::new(),
                    }))
                }
                _ => Ok(None),
            }
        }
        EventKind::RetryRequested { actor, reason } => {
            Ok(Some(BriefEvent::RetryRequested { actor, reason }))
        }
        _ => Ok(None),
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

fn redis_value_as_str(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
        redis::Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}
