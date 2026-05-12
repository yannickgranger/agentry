//! Per-brief surgical abort path for the `orchestrator` CLI.
//!
//! Pushes `BriefEvent::AbortRequested` onto the brief's trace stream so
//! the per-brief lifecycle driver consumes it and the universal aborts
//! handler in `orchestrator_types::lifecycle::handle` transitions any
//! non-terminal state to `Failed { AbortRequested }`. Idempotent on
//! terminals: an already-`Shipped` or `Failed` brief returns
//! `status: "already_terminal"` without touching Redis or the host.
//!
//! Cleanup is best-effort: a bogus / missing-container `agent_id` warns
//! and proceeds, never aborting the FSM event push (correctness path).
//! Workspace teardown and trace-stream pruning are gated by the
//! `keep_workspace` / `keep_trace` flags so an operator can preserve
//! forensics without losing the surgical-shutdown idempotency.
//!
//! Pre-condition: a live per-brief lifecycle driver is consuming the
//! brief's trace stream. If the driver is dead (e.g. a daemon restart
//! happened pre-471b reattach), the `AbortRequested` event sits in the
//! trace stream unconsumed and the operator must fall back to a direct
//! Redis `:state` patch. This is documented in
//! `specs/concepts/brief_lifecycle.md` under "Operator abort".

use crate::workspace::BriefWorkspace;
use crate::{daemon_resume::brief_state_agent_id, redis_io, Config, Error, Result};
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord};
use orchestrator_types::{BriefId, Event, EventKind};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::time::Duration;

/// `agent` field used on the trace-stream entry that carries the
/// CLI-pushed `AbortRequested` event. Operators grepping the stream for
/// the surgical-abort signal match on this exact string — distinct from
/// `wall-clock-reaper` (the budget reaper) and `daemon` (the
/// handler-error abort raised inside the per-brief task).
pub const ABORT_AGENT_ID: &str = "operator-cli";

/// Default delay between pushing `AbortRequested` and pruning the trace
/// stream — the per-brief driver needs time to consume the event and
/// project the resulting `Failed{AbortRequested}` state record before
/// the trace stream is gone.
pub const POST_ABORT_PRUNE_DELAY: Duration = Duration::from_millis(750);

/// Push `BriefEvent::AbortRequested` onto `brief_id`'s trace stream and
/// best-effort tear down its container + workspace + trace stream.
///
/// Returns:
/// - `Ok(())` on success (event pushed, or the brief was already in a
///   terminal state — both are operator-visible "the brief is no
///   longer running" outcomes).
/// - `Err(Error::NotFound { kind: "brief", .. })` when no `:state`
///   key exists for `brief_id`. The CLI maps this onto a
///   `no such brief: {id}` stderr message + exit 1.
/// - `Err(Error::Config(..))` for malformed `brief_id` strings.
/// - `Err(Error::Redis | Error::Json | ..)` for backend / serialization
///   failures.
///
/// # Errors
///
/// See the return-value table above. Best-effort steps (container
/// stop/kill, workspace remove, trace prune, state_log delete) are NOT
/// promoted to `Err` — they log at `warn!` and the function returns
/// `Ok(())`, on the principle that the FSM event push (the
/// correctness-critical step) has already landed.
pub async fn run_per_brief_abort(
    cfg: &Config,
    brief_id: &str,
    keep_workspace: bool,
    keep_trace: bool,
) -> Result<()> {
    validate_brief_id(brief_id)?;

    let mut conn = redis_io::connect(&cfg.redis.url).await?;

    let state_key = format!("agentry:brief:{brief_id}:state");
    let raw: Option<String> = conn.get(&state_key).await?;
    let Some(raw) = raw else {
        return Err(Error::NotFound {
            kind: "brief",
            key: brief_id.to_string(),
        });
    };
    let record: BriefStateRecord = serde_json::from_str(&raw)?;

    if let Some(reason_kind) = terminal_reason(&record.state) {
        println!(
            "{{\"aborted\":true,\"brief\":\"{brief_id}\",\"mode\":\"per_brief\",\"status\":\"already_terminal\",\"reason\":\"{reason_kind}\"}}"
        );
        return Ok(());
    }

    push_abort_event(&mut conn, brief_id).await?;

    if let Some(agent_id) = brief_state_agent_id(&record.state) {
        kill_container(agent_id).await;
    } else {
        tracing::info!(
            brief = %brief_id,
            state_kind = %state_kind_str(&record.state),
            "abort: no agent_id on current state, skipping container stop"
        );
    }

    if !keep_workspace {
        remove_workspace_dir(brief_id).await;
    }

    if !keep_trace {
        // Give the per-brief driver a moment to consume AbortRequested
        // and project the resulting Failed{AbortRequested} record. The
        // verdict has already been recorded in agentry:verdicts at
        // that point, so the trace stream is no longer load-bearing
        // for the FSM.
        tokio::time::sleep(POST_ABORT_PRUNE_DELAY).await;
        prune_trace_streams(&mut conn, brief_id).await;
    }

    println!(
        "{{\"aborted\":true,\"brief\":\"{brief_id}\",\"mode\":\"per_brief\",\"status\":\"requested\",\"keep_workspace\":{keep_workspace},\"keep_trace\":{keep_trace}}}"
    );
    Ok(())
}

/// Validate `brief_id` shape: must be `brf_` prefixed, ASCII, with no
/// embedded whitespace or path separators (the value is interpolated
/// into Redis keys and `podman ps --filter name=` arguments, so
/// rejecting `:`/`/`/space at the door avoids both Redis namespace
/// collisions and shell-arg surprises).
fn validate_brief_id(brief_id: &str) -> Result<()> {
    if !brief_id.starts_with("brf_") {
        return Err(Error::Config(format!(
            "invalid brief_id {brief_id:?}: must start with 'brf_'"
        )));
    }
    if brief_id.len() < 5 {
        return Err(Error::Config(format!(
            "invalid brief_id {brief_id:?}: empty body after 'brf_'"
        )));
    }
    let bad = brief_id
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    if bad {
        return Err(Error::Config(format!(
            "invalid brief_id {brief_id:?}: only [A-Za-z0-9_-] allowed after 'brf_'"
        )));
    }
    Ok(())
}

fn terminal_reason(state: &BriefState) -> Option<&'static str> {
    match state {
        BriefState::Shipped => Some("shipped"),
        BriefState::Failed { .. } => Some("failed"),
        _ => None,
    }
}

fn state_kind_str(state: &BriefState) -> &'static str {
    match state {
        BriefState::Submitted => "submitted",
        BriefState::Authoring { .. } => "authoring",
        BriefState::Verifying { .. } => "verifying",
        BriefState::Reviewing { .. } => "reviewing",
        BriefState::Reworking { .. } => "reworking",
        BriefState::Shipping { .. } => "shipping",
        BriefState::Watching { .. } => "watching",
        BriefState::Extension { .. } => "extension",
        BriefState::AwaitingCaptainDecision { .. } => "awaiting_captain_decision",
        BriefState::Walking { .. } => "walking",
        BriefState::Shipped => "shipped",
        BriefState::Failed { .. } => "failed",
    }
}

async fn push_abort_event(conn: &mut ConnectionManager, brief_id: &str) -> Result<()> {
    let abort = BriefEvent::AbortRequested {
        actor: "operator".to_string(),
        message: "orchestrator abort cli".to_string(),
    };
    let payload = serde_json::to_value(&abort)?;
    let event = Event::new(EventKind::Event { payload });
    let body = serde_json::to_string(&event)?;
    let stream = format!("agentry:brief:{brief_id}:trace");
    let _: String = conn
        .xadd(
            &stream,
            "*",
            &[("agent", ABORT_AGENT_ID), ("event", body.as_str())],
        )
        .await?;
    Ok(())
}

async fn kill_container(agent_id: &str) {
    let name = format!("agentry-{agent_id}");
    let stop = tokio::process::Command::new("podman")
        .args(["stop", "-t", "1", &name])
        .output()
        .await;
    match stop {
        Ok(o) if o.status.success() => {
            tracing::info!(agent = %agent_id, container = %name, "abort: stopped container");
            return;
        }
        Ok(o) => {
            tracing::warn!(
                agent = %agent_id,
                container = %name,
                status = ?o.status,
                stderr = %String::from_utf8_lossy(&o.stderr),
                "abort: podman stop failed; falling through to kill"
            );
        }
        Err(e) => {
            tracing::warn!(
                agent = %agent_id,
                container = %name,
                error = %e,
                "abort: podman stop spawn failed; falling through to kill"
            );
        }
    }
    let kill = tokio::process::Command::new("podman")
        .args(["kill", &name])
        .output()
        .await;
    match kill {
        Ok(o) if o.status.success() => {
            tracing::info!(agent = %agent_id, container = %name, "abort: killed container");
        }
        Ok(o) => {
            tracing::warn!(
                agent = %agent_id,
                container = %name,
                status = ?o.status,
                stderr = %String::from_utf8_lossy(&o.stderr),
                "abort: podman kill failed (container may already be gone)"
            );
        }
        Err(e) => {
            tracing::warn!(
                agent = %agent_id,
                container = %name,
                error = %e,
                "abort: podman kill spawn failed"
            );
        }
    }
}

async fn remove_workspace_dir(brief_id: &str) {
    let path = BriefWorkspace::root().join("briefs").join(brief_id);
    match tokio::fs::metadata(&path).await {
        Ok(_) => {}
        Err(_) => {
            tracing::info!(brief = %brief_id, path = %path.display(), "abort: workspace dir absent, nothing to remove");
            return;
        }
    }
    match tokio::fs::remove_dir_all(&path).await {
        Ok(()) => {
            tracing::info!(brief = %brief_id, path = %path.display(), "abort: removed workspace dir");
        }
        Err(e) => {
            tracing::warn!(
                brief = %brief_id,
                path = %path.display(),
                error = %e,
                "abort: failed to remove workspace dir"
            );
        }
    }
}

async fn prune_trace_streams(conn: &mut ConnectionManager, brief_id: &str) {
    let trace_key = format!("agentry:brief:{brief_id}:trace");
    let log_key = format!("agentry:brief:{brief_id}:state_log");
    match redis::cmd("XTRIM")
        .arg(&trace_key)
        .arg("MAXLEN")
        .arg(0)
        .query_async::<i64>(conn)
        .await
    {
        Ok(trimmed) => {
            tracing::info!(brief = %brief_id, trimmed, "abort: pruned trace stream");
        }
        Err(e) => {
            tracing::warn!(brief = %brief_id, error = %e, "abort: XTRIM trace failed");
        }
    }
    match conn.del::<_, i64>(&log_key).await {
        Ok(deleted) => {
            tracing::info!(brief = %brief_id, deleted, "abort: deleted state_log");
        }
        Err(e) => {
            tracing::warn!(brief = %brief_id, error = %e, "abort: DEL state_log failed");
        }
    }
}

/// Build a `BriefId` from a validated `brief_id` string. Centralises the
/// `BriefId(...)` newtype hop so callers (and the test suite) never
/// hand-construct the inner field.
#[must_use]
pub fn brief_id_from_str(brief_id: &str) -> BriefId {
    BriefId(brief_id.to_string())
}
