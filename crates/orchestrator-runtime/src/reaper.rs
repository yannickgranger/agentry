//! Wall-clock reaper for non-terminal lifecycle states.
//!
//! Side task spawned at daemon boot. Every
//! [`REAPER_INTERVAL_SECONDS`] it scans `agentry:brief:*:state`,
//! identifies non-terminal [`BriefStateRecord`]s whose
//! `now() - record.at` has crossed the brief's `budget.max_wall_seconds`
//! (or the daemon-level [`DEFAULT_WALL_CLOCK_SECONDS`] fallback), and:
//!
//! 1. Pushes a [`BriefEvent::BudgetExhausted`] into the brief's trace
//!    stream — the per-brief lifecycle driver's
//!    [`crate::lifecycle::EventSource`] picks it up and the FSM
//!    transitions to `BriefState::Failed { reason: BudgetExhausted }`.
//! 2. Best-effort `podman kill` of any container labeled
//!    `agentry.brief={id}` so the orphan stops burning tokens.
//!
//! Closes Cases 2/3/4 in `docs/forensics/orphan_pattern.md` (PRs
//! #374, #381, #382 from session 2026-05-04) where containers died
//! before their terminal `Done` event, leaving briefs frozen in a
//! non-terminal state with no recovery path.

use crate::lifecycle::EventSourceError;
use async_trait::async_trait;
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord};
use orchestrator_types::{BriefId, Event, EventKind, Ts};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::Value as JsonValue;
use std::time::Duration;

/// How often the reaper sweep runs. Council-locked at 30s — the
/// detection window for an orphan brief should be small relative to
/// typical brief budgets (≥10× the 5-min default).
pub const REAPER_INTERVAL_SECONDS: u64 = 30;

/// Daemon-level fallback wall-clock budget for briefs that did not
/// declare `payload.budget.max_wall_seconds`. 30 minutes — covers the
/// long tail of legacy briefs while still bounded enough that an
/// orphan does not languish for hours.
pub const DEFAULT_WALL_CLOCK_SECONDS: u64 = 1800;

/// The trace-stream `agent` field used when the reaper pushes its
/// budget-exhaustion event. Operators grepping the stream for the
/// reaper's signal match on this exact string.
pub const REAPER_AGENT_ID: &str = "wall-clock-reaper";

/// True iff `record` is non-terminal AND `now - record.at >
/// budget_seconds`. Strict greater-than: a record exactly at the
/// budget is NOT orphan (to avoid double-fire on a freshly-stamped
/// boundary), one second over is. Terminal states (`Shipped`,
/// `Failed`) are never orphan regardless of elapsed time.
#[must_use]
pub fn is_orphan(record: &BriefStateRecord, now: Ts, budget_seconds: u64) -> bool {
    if matches!(record.state, BriefState::Shipped | BriefState::Failed { .. }) {
        return false;
    }
    let elapsed = now.signed_duration_since(record.at).num_seconds();
    if elapsed < 0 {
        return false;
    }
    (elapsed as u64) > budget_seconds
}

/// Read port: yields every in-flight brief's latest
/// [`BriefStateRecord`] plus that brief's declared
/// `budget.max_wall_seconds`. Production scans
/// `agentry:brief:*:state` keys; tests inject deterministic fixtures.
#[async_trait]
pub trait BriefInventory: Send + Sync {
    /// Yield every brief's latest [`BriefStateRecord`]. Order is
    /// unspecified — the reaper does not depend on it. Implementations
    /// SHOULD log and skip individual malformed entries rather than
    /// fail the whole sweep.
    async fn list_state_records(&mut self) -> Result<Vec<BriefStateRecord>, EventSourceError>;

    /// Read `payload.budget.max_wall_seconds` from the brief's body
    /// key. Returns `Ok(None)` when the brief has no body, no budget,
    /// or the budget field is absent — caller falls back to
    /// [`DEFAULT_WALL_CLOCK_SECONDS`].
    async fn read_max_wall_seconds(
        &mut self,
        brief_id: &BriefId,
    ) -> Result<Option<u64>, EventSourceError>;
}

/// Write port: side-effects the reaper invokes once it has classified
/// a brief as orphan. The two effects are decoupled: trace-stream push
/// is correctness-critical (drives the FSM), `podman kill` is
/// best-effort (stops the runaway container).
#[async_trait]
pub trait ReaperSink: Send + Sync {
    /// Push `event` into the brief's trace stream. The per-brief
    /// lifecycle driver's [`crate::lifecycle::EventSource`] picks it
    /// up and runs `handle()` to transition the FSM.
    async fn push_event(
        &mut self,
        brief_id: &BriefId,
        event: &BriefEvent,
    ) -> Result<(), EventSourceError>;

    /// Best-effort: kill any running podman containers labeled
    /// `agentry.brief={id}`. Implementations log on failure but never
    /// propagate — the FSM transition above is the source of truth,
    /// the container kill just stops the token burn faster.
    async fn kill_containers(&mut self, brief_id: &BriefId);
}

/// Spawn-friendly entry point: tick every `interval` and never
/// returns. Errors from individual sweeps are logged at WARN —
/// the next tick re-runs the scan from scratch, so a transient
/// Redis hiccup doesn't lose orphans.
pub async fn run<I, S>(
    mut inventory: I,
    mut sink: S,
    default_budget_seconds: u64,
    interval: Duration,
) where
    I: BriefInventory + 'static,
    S: ReaperSink + 'static,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(
        interval_secs = interval.as_secs(),
        default_budget_secs = default_budget_seconds,
        "wall-clock reaper started"
    );
    loop {
        ticker.tick().await;
        match tick(
            &mut inventory,
            &mut sink,
            default_budget_seconds,
            orchestrator_types::now(),
        )
        .await
        {
            Ok(reaped) if reaped > 0 => {
                tracing::info!(reaped, "reaper tick: orphan briefs reaped");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "reaper tick failed; will retry next interval");
            }
        }
    }
}

/// One reaper sweep. Pure on `(inventory, sink, budget, now)` —
/// extracted from [`run`] so tests can drive the loop deterministically
/// without spinning up a real `tokio::time::interval`. Returns the
/// count of orphans reaped this sweep.
pub async fn tick<I, S>(
    inventory: &mut I,
    sink: &mut S,
    default_budget_seconds: u64,
    now: Ts,
) -> Result<usize, EventSourceError>
where
    I: BriefInventory,
    S: ReaperSink,
{
    let records = inventory.list_state_records().await?;
    let mut reaped = 0usize;
    for record in records {
        let budget = inventory
            .read_max_wall_seconds(&record.brief_id)
            .await?
            .unwrap_or(default_budget_seconds);
        if is_orphan(&record, now, budget) {
            tracing::warn!(
                brief = %record.brief_id.0,
                budget_seconds = budget,
                state = ?record.state,
                "wall-clock reaper: brief exceeded budget — pushing BudgetExhausted"
            );
            sink.push_event(&record.brief_id, &BriefEvent::BudgetExhausted)
                .await?;
            sink.kill_containers(&record.brief_id).await;
            reaped += 1;
        }
    }
    Ok(reaped)
}

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
                .await?;
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
            let raw: Option<String> = self.conn.get(&key).await?;
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
        let raw: Option<String> = self.conn.get(&key).await?;
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
            .await?;
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
