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

use crate::lifecycle_ports::EventSourceError;
use async_trait::async_trait;
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord};
use orchestrator_types::{BriefId, Ts};
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

/// Stale-trace probe threshold: a non-terminal brief whose trace
/// stream has been silent longer than this is considered an orphan.
/// 10 min default — long enough to absorb a slow cargo build pulse,
/// short enough to catch orphan-after-coder-commit before the 30-min
/// wall-clock fires. Do NOT shorten below 600s without forensics —
/// cargo cold builds can be quiet for 5+ min.
pub const STALE_TRACE_THRESHOLD_SECONDS: u64 = 600;

/// True iff `record` is non-terminal AND `now - record.at >
/// budget_seconds`. Strict greater-than: a record exactly at the
/// budget is NOT orphan (to avoid double-fire on a freshly-stamped
/// boundary), one second over is. Terminal states (`Shipped`,
/// `Failed`) are never orphan regardless of elapsed time.
#[must_use]
pub fn is_orphan(record: &BriefStateRecord, now: Ts, budget_seconds: u64) -> bool {
    if matches!(
        record.state,
        BriefState::Shipped | BriefState::Failed { .. }
    ) {
        return false;
    }
    let elapsed = now.signed_duration_since(record.at).num_seconds();
    if elapsed < 0 {
        return false;
    }
    (elapsed as u64) > budget_seconds
}

/// True iff `record` is non-terminal, NOT
/// [`BriefState::AwaitingCaptainDecision`] (operator-gated; expected
/// to be quiet), AND the brief's trace stream has been silent longer
/// than `threshold`. Terminal states (`Shipped`, `Failed`) return
/// false. Companion probe to [`is_orphan`] — wall-clock budget elapse
/// vs. trace-quiet are the two orphan signals the reaper acts on.
#[must_use]
pub fn is_trace_orphan(record: &BriefStateRecord, trace_age_seconds: u64, threshold: u64) -> bool {
    match record.state {
        BriefState::Shipped | BriefState::Failed { .. } => false,
        BriefState::AwaitingCaptainDecision { .. } => false,
        _ => trace_age_seconds > threshold,
    }
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

    /// Read the age (in seconds) of the most recent entry on the
    /// brief's trace stream `agentry:brief:<id>:trace`. The production
    /// adapter reads the last XID timestamp via `XINFO STREAM`'s
    /// `last-generated-id` field and subtracts from `now`. Returns
    /// `Ok(None)` when the trace stream is empty or absent — the
    /// caller treats `None` as not-orphan (a brief with no trace yet
    /// is brand new, not stuck).
    async fn last_trace_event_age(
        &mut self,
        brief_id: &BriefId,
        now: Ts,
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
                kind = "wall_clock_orphan",
                brief = %record.brief_id.0,
                budget_seconds = budget,
                state = ?record.state,
                "wall-clock reaper: brief exceeded budget — pushing BudgetExhausted"
            );
            sink.push_event(&record.brief_id, &BriefEvent::BudgetExhausted)
                .await?;
            sink.kill_containers(&record.brief_id).await;
            reaped += 1;
            continue;
        }
        if let Some(age) = inventory
            .last_trace_event_age(&record.brief_id, now)
            .await?
        {
            if is_trace_orphan(&record, age, STALE_TRACE_THRESHOLD_SECONDS) {
                tracing::warn!(
                    kind = "stale_trace_orphan",
                    brief = %record.brief_id.0,
                    trace_age_seconds = age,
                    threshold_seconds = STALE_TRACE_THRESHOLD_SECONDS,
                    state = ?record.state,
                    "stale-trace reaper: brief trace silent past threshold — pushing BudgetExhausted"
                );
                sink.push_event(&record.brief_id, &BriefEvent::BudgetExhausted)
                    .await?;
                sink.kill_containers(&record.brief_id).await;
                reaped += 1;
            }
        }
    }
    Ok(reaped)
}
