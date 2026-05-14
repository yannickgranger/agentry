//! Lifecycle FSM driver task per brief.
//!
//! The daemon spawns one of these tasks per brief. The task drives the
//! lifecycle FSM (`orchestrator_types::lifecycle::handle`) by pulling
//! `BriefEvent`s from an [`EventSource`], applying each one, and writing
//! the resulting [`BriefStateRecord`] via a [`StateProjector`]. On
//! reaching a terminal state (`Shipped` or `Failed`), the task XADDs a
//! [`Verdict`] to `agentry:verdicts`. The terminal-state transition is
//! single by construction, so this is the sole writer to the verdicts
//! stream.

use crate::lifecycle::{
    EventSource, StateProjector, NO_OP_SHORT_CIRCUIT_CAUSE, NO_OP_VERDICT_REASON,
};
use crate::redis_io;
use crate::workspace::{self, BriefWorkspace, TerminationDisposition};
use crate::{Error, Result};
use orchestrator_types::lifecycle::{handle, BriefEvent, BriefState, BriefStateRecord};
use orchestrator_types::{now, BriefId, Event, EventKind, Verdict, VerdictKind};
use redis::aio::ConnectionManager;
use std::path::Path;

// The daemon's per-brief lifecycle factories are spelled out inline in
// `daemon::run`'s parameter list — type-aliasing them here would surface
// new pub items not covered by `specs/concepts/brief_state_stream.md`,
// which is ratified for L.2 / L.3a and intentionally not amended in
// this slice. Callers in the orchestratord binary construct the values
// via `Arc::new(move |brief_id| Box::new(...))` and rely on inference.

/// Drive the lifecycle FSM for a single brief.
///
/// Pulls events from `source` and walks
/// `orchestrator_types::lifecycle::handle` from `BriefState::Submitted`
/// to a terminal state, writing each valid transition through
/// `projector`. Events the FSM rejects in the current state are logged
/// at WARN and skipped — the FSM is the source of truth for what is
/// legal, the source may yield events that race the FSM's view of the
/// brief.
///
/// On reaching a terminal state, also emits a [`Verdict`] to
/// `agentry:verdicts` via `verdict_conn` (when supplied). The
/// production caller passes `Some(conn)`; in-process tests pass `None`
/// to exercise the projector pipeline without a Redis dependency.
///
/// The cursor written by `projector.write` is a synthetic `step-N`
/// counter local to this driver — the spec invariant is that the
/// cursor advances per processed event, which the counter satisfies.
/// L.3b will replace it with the real trace-stream entry id once the
/// L.2 adapter exposes its internal cursor.
pub async fn projector_task(
    brief_id: BriefId,
    mut source: Box<dyn EventSource + Send>,
    mut projector: Box<dyn StateProjector + Send>,
    mut verdict_conn: Option<ConnectionManager>,
    walk_config: std::sync::Arc<orchestrator_types::lifecycle::WalkConfig>,
    entry_node: std::sync::Arc<orchestrator_types::NodeId>,
) -> Result<()> {
    let mut state = BriefState::Submitted;
    let mut step: u64 = 0;
    let mut no_op_reason: Option<String> = None;
    loop {
        let event = match source
            .next()
            .await
            .map_err(|e| Error::Config(format!("event source: {e}")))?
        {
            Some(ev) => ev,
            None => break,
        };
        // Latch the no-op short-circuit reason BEFORE FSM dispatch so
        // the terminal Shipped verdict carries the operator-visible
        // text instead of the generic "fsm: shipped".
        if let BriefEvent::CoderDoneNoOp { reason } = &event {
            no_op_reason = Some(reason.clone());
        }
        step = step.saturating_add(1);
        let cursor = format!("step-{step}");

        match handle(&state, &event, &walk_config, &entry_node) {
            Ok(new_state) => {
                let record = BriefStateRecord {
                    brief_id: brief_id.clone(),
                    state: new_state.clone(),
                    parent_brief_id: None,
                    composition_role: None,
                    at: now(),
                };
                projector
                    .write(&record, &cursor)
                    .await
                    .map_err(|e| Error::Config(format!("state projector: {e}")))?;
                state = new_state;
                if matches!(state, BriefState::Shipped | BriefState::Failed { .. }) {
                    if let Some(ref mut conn) = verdict_conn {
                        emit_terminal_verdict(conn, &brief_id, &state, no_op_reason.as_deref())
                            .await?;
                    }
                    if matches!(state, BriefState::Failed { .. }) {
                        cleanup_failed_brief(&brief_id, verdict_conn.as_mut()).await;
                    } else if no_op_reason.is_some() {
                        // No-op short-circuit: the daemon's
                        // handle_brief never reaches finalize_shipped_team
                        // (it only sees the coder's outcome and there
                        // was no fan-out), so the workspace teardown
                        // must fire here. Uses the dedicated no-op
                        // helper so the log line and Redis trace event
                        // identify the cause as "no-op short-circuit"
                        // rather than mislabeling the audit log with
                        // "terminal Failed" wording.
                        cleanup_shipped_no_op_brief(&brief_id, verdict_conn.as_mut()).await;
                    }
                    break;
                }
            }
            Err(invalid) => {
                // Late-event fence: a RoleDone for a node that was
                // already passed by the walker is silently dropped at
                // the FSM layer (handle() returns state unchanged via
                // is_late_event). If the FSM rejects a RoleDone with
                // InvalidTransition, that's a genuinely-late event
                // arriving when the walker has progressed past its
                // dependent gate — warn and continue rather than fail.
                let is_late_role_done =
                    matches!(invalid.event, BriefEvent::RoleDone { .. })
                        && matches!(invalid.from, BriefState::Walking { .. });
                if is_late_role_done {
                    tracing::warn!(
                        brief = %brief_id.0,
                        from = ?invalid.from,
                        event = ?invalid.event,
                        outcome = "dropped_late_role_done_in_walking",
                        "FSM rejected late RoleDone in Walking; dropping event without state transition"
                    );
                    continue;
                }
                tracing::error!(
                    brief = %brief_id.0,
                    from = ?invalid.from,
                    event = ?invalid.event,
                    "FSM rejected event; failing brief with DaemonError"
                );
                let detail = format!(
                    "FSM rejected event {:?} in state {:?}",
                    invalid.event, invalid.from
                );
                let failed_state = BriefState::Failed {
                    reason: orchestrator_types::lifecycle::Reason::DaemonError { detail },
                };
                let record = BriefStateRecord {
                    brief_id: brief_id.clone(),
                    state: failed_state.clone(),
                    parent_brief_id: None,
                    composition_role: None,
                    at: now(),
                };
                projector
                    .write(&record, &cursor)
                    .await
                    .map_err(|e| Error::Config(format!("state projector: {e}")))?;
                state = failed_state;
                if let Some(ref mut conn) = verdict_conn {
                    emit_terminal_verdict(conn, &brief_id, &state, no_op_reason.as_deref()).await?;
                }
                cleanup_failed_brief(&brief_id, verdict_conn.as_mut()).await;
                break;
            }
        }
    }
    Ok(())
}

/// Emit the terminal [`Verdict`] for a brief from the FSM driver. The
/// terminal-state transition is single by construction, so this is the
/// sole writer to `agentry:verdicts`.
async fn emit_terminal_verdict(
    conn: &mut ConnectionManager,
    brief_id: &BriefId,
    state: &BriefState,
    no_op_reason: Option<&str>,
) -> Result<()> {
    let (kind, reason) = match state {
        BriefState::Shipped => match no_op_reason {
            Some(text) if !text.is_empty() => (VerdictKind::Shipped, text.to_string()),
            _ if no_op_reason.is_some() => (VerdictKind::Shipped, NO_OP_VERDICT_REASON.to_string()),
            _ => (VerdictKind::Shipped, "fsm: shipped".to_string()),
        },
        BriefState::Failed { reason } => (VerdictKind::Failed, format!("fsm: {reason:?}")),
        // projector_task only invokes this on terminal states, so this
        // arm is unreachable in practice. Returning Ok keeps the function
        // total without panicking.
        _ => return Ok(()),
    };
    let verdict = Verdict::new(brief_id.clone(), kind).with_reason(reason);
    let stream_id = redis_io::append_verdict(conn, &verdict).await?;
    tracing::info!(
        brief = %verdict.brief.0,
        kind = ?verdict.kind,
        stream_id = %stream_id,
        "fsm: terminal verdict emitted"
    );
    Ok(())
}

/// Which terminal disposition triggered the workspace cleanup. Drives
/// the wording of the tracing logs and the Redis trace event so the
/// audit log truthfully labels successful no-op short-circuits separate
/// from terminal Failed cleanups (the two paths are otherwise
/// mechanically identical — same workspace dir tear-down, same
/// idempotency / error-swallowing semantics).
#[derive(Debug, Clone, Copy)]
enum CleanupDisposition {
    /// Cleanup fired because the FSM reached `BriefState::Failed`.
    Failed,
    /// Cleanup fired because the FSM short-circuited
    /// `Authoring → Shipped` on a no-op brief (acceptance passed but
    /// the coder's diff against base was empty).
    ShippedNoOp,
}

impl CleanupDisposition {
    /// Operator-facing wording for the disposition, used in both the
    /// `tracing` info/warn lines and the `msg` field of the Redis
    /// trace event appended at cleanup. Kept short — fits inline in
    /// log scans and `agentry:brief:<id>:trace` tails.
    fn label(self) -> &'static str {
        match self {
            Self::Failed => "terminal Failed",
            Self::ShippedNoOp => "no-op short-circuit",
        }
    }

    /// Machine-greppable cause sentinel emitted as a structured
    /// `cause` field on the no-op cleanup trace event so operators
    /// scanning `agentry:brief:<id>:trace` for the
    /// `NO_OP_SHORT_CIRCUIT_CAUSE` constant value find both the
    /// coder's terminal `Done` event and this cleanup event without
    /// having to also know the human-readable disposition wording.
    /// `None` for `Failed` — the existing `terminal Failed` label
    /// already pins that disposition unambiguously.
    fn cause_sentinel(self) -> Option<&'static str> {
        match self {
            Self::Failed => None,
            Self::ShippedNoOp => Some(NO_OP_SHORT_CIRCUIT_CAUSE),
        }
    }
}

/// On terminal `BriefState::Failed`, tear down the brief's per-brief
/// clone dir, and (when `conn` is provided) append a trace event
/// recording the cleanup.
///
/// Replaces the prior "retain on failure for audit" rule: failures are
/// reconstructable from Redis (the trace stream and state log are the
/// audit log), and retained workspaces produced ~6 dispatch-blocking
/// stale-worktree incidents in the EPIC #255/#256 drain.
///
/// Idempotent — `workspace::destroy` treats a missing dir as a no-op.
/// Errors are logged and swallowed: cleanup failure must not crash the
/// projector_task at the terminal step.
///
/// The thin wrapper resolves the workspace root from the
/// `AGENTRY_WORKSPACE_ROOT` env var (or its compiled-in default) and
/// delegates to [`cleanup_failed_brief_at`]. Tests should call
/// [`cleanup_failed_brief_at`] directly with an explicit root so they
/// don't race other tests on the process-wide env var.
pub async fn cleanup_failed_brief(brief_id: &BriefId, conn: Option<&mut ConnectionManager>) {
    cleanup_brief_at(
        brief_id,
        &BriefWorkspace::root(),
        CleanupDisposition::Failed,
        conn,
    )
    .await;
}

/// Test-friendly variant of [`cleanup_failed_brief`] that takes an
/// explicit workspace root rather than reading
/// `AGENTRY_WORKSPACE_ROOT`. Production code should call the wrapper.
pub async fn cleanup_failed_brief_at(
    brief_id: &BriefId,
    root: &Path,
    conn: Option<&mut ConnectionManager>,
) {
    cleanup_brief_at(brief_id, root, CleanupDisposition::Failed, conn).await;
}

/// On a no-op short-circuit `BriefState::Shipped`, tear down the
/// brief's per-brief clone dir, and (when `conn` is provided) append
/// a trace event truthfully labeled `"workspace cleaned (no-op
/// short-circuit)"`.
///
/// The cleanup mechanics are identical to [`cleanup_failed_brief`]
/// (same `workspace::destroy_with_disposition` call, same idempotency,
/// same error-swallowing semantics) — only the audit-log wording
/// differs. Lives as a sibling helper rather than a parameter on the
/// existing function so callers cannot accidentally tag a Failed
/// cleanup as a no-op or vice-versa: the disposition is fixed at the
/// call site by the function name.
pub async fn cleanup_shipped_no_op_brief(brief_id: &BriefId, conn: Option<&mut ConnectionManager>) {
    cleanup_brief_at(
        brief_id,
        &BriefWorkspace::root(),
        CleanupDisposition::ShippedNoOp,
        conn,
    )
    .await;
}

/// Test-friendly variant of [`cleanup_shipped_no_op_brief`] that takes
/// an explicit workspace root rather than reading
/// `AGENTRY_WORKSPACE_ROOT`. Production code should call the wrapper.
pub async fn cleanup_shipped_no_op_brief_at(
    brief_id: &BriefId,
    root: &Path,
    conn: Option<&mut ConnectionManager>,
) {
    cleanup_brief_at(brief_id, root, CleanupDisposition::ShippedNoOp, conn).await;
}

/// Shared body of the per-disposition cleanup helpers. The disposition
/// only influences the wording of the tracing log lines and the `msg`
/// field of the Redis trace event — the workspace dir teardown,
/// idempotency, and error-swallowing behavior are identical across
/// dispositions.
#[tracing::instrument(skip_all, fields(brief = %brief_id.0, disposition = %disposition.label()))]
async fn cleanup_brief_at(
    brief_id: &BriefId,
    root: &Path,
    disposition: CleanupDisposition,
    conn: Option<&mut ConnectionManager>,
) {
    let host_path = root.join("briefs").join(&brief_id.0);
    let ws = BriefWorkspace {
        brief_id: brief_id.clone(),
        host_path,
    };
    let label = disposition.label();
    if let Err(e) = workspace::destroy_with_disposition(&ws, TerminationDisposition::TearDown).await
    {
        tracing::warn!(
            brief = %brief_id.0,
            error = %e,
            disposition = label,
            "workspace cleanup failed"
        );
    } else {
        tracing::info!(
            brief = %brief_id.0,
            path = %ws.host_path.display(),
            disposition = label,
            "workspace cleaned"
        );
    }
    if let Some(conn) = conn {
        let mut payload = serde_json::json!({
            "msg": format!("workspace cleaned ({label})"),
            "brief_id": brief_id.0,
            "disposition": label,
        });
        if let Some(cause) = disposition.cause_sentinel() {
            payload["cause"] = serde_json::Value::String(cause.to_string());
        }
        let event = Event::new(EventKind::Event { payload });
        if let Err(e) = redis_io::append_trace(conn, brief_id, "lifecycle-driver", &event).await {
            tracing::warn!(
                brief = %brief_id.0,
                error = %e,
                "trace append for workspace-cleanup event failed"
            );
        }
        apply_terminal_ttl(conn, brief_id).await;
    }
}

/// Retention window applied to every `agentry:brief:{id}:*` sibling key
/// when a brief reaches a terminal disposition (Failed or no-op
/// Shipped). 30 days = 2_592_000 seconds. Long enough for operator
/// forensics, short enough to keep Redis bounded in steady state.
/// Single source of truth per the "no metric ratchets" rule generalised
/// to retention: tunable here, no env var, no allowlist.
pub const TERMINAL_BRIEF_TTL_SECONDS: usize = 30 * 24 * 60 * 60;

/// Best-effort TTL pass over every `agentry:brief:{brief_id}:*` sibling
/// key. Discovers keys via `SCAN` (future-proof against new sibling keys
/// landing without an audit of this list) and sets `EXPIRE` on each to
/// [`TERMINAL_BRIEF_TTL_SECONDS`]. Failures (key already gone, transient
/// Redis blip) are logged at DEBUG and skipped — TTL is best-effort and
/// must not block the FSM driver's terminal step.
#[tracing::instrument(skip_all, fields(brief = %brief_id.0))]
async fn apply_terminal_ttl(conn: &mut ConnectionManager, brief_id: &BriefId) {
    let pattern = format!("agentry:brief:{}:*", brief_id.0);
    let mut cursor: u64 = 0;
    loop {
        let scan: redis::RedisResult<(u64, Vec<String>)> = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(100)
            .query_async(conn)
            .await;
        let (next, batch) = match scan {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    brief = %brief_id.0,
                    error = %e,
                    "terminal TTL SCAN failed; skipping remainder"
                );
                return;
            }
        };
        for key in batch {
            let res: redis::RedisResult<i64> = redis::cmd("EXPIRE")
                .arg(&key)
                .arg(TERMINAL_BRIEF_TTL_SECONDS)
                .query_async(conn)
                .await;
            if let Err(e) = res {
                tracing::debug!(
                    brief = %brief_id.0,
                    key = %key,
                    error = %e,
                    "terminal TTL EXPIRE failed; skipping"
                );
            }
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
}

/// Project a team's topology into the per-node `WalkConfig` consumed by
/// the lifecycle DAG walker. Beta-b deleted the legacy `build_phase_gates`
/// helper and routes the FSM through the walker built from this output.
///
/// Adjacency: walks `team.message_graph` and groups each `MessageEdge` by
/// `from`, with the `to` collected as `NodeId(to.name.0)`.
///
/// Per-node configs: walks `team.roles`, then for each role
///   - `class` is `team.node_classes.get(role.name)`, falling back to
///     `NodeClass("container_bound")` when the role is not pinned in
///     `node_classes` (legacy topologies and operator-gated vertices);
///   - `expected_inbound` is the deduplicated upstream role list from
///     `team.inbound_roles`, mapped to `NodeId`;
///   - `policy` is the first inbound edge's `gate_policy` if any are
///     pinned, falling back to `GatePolicy::AllMustPass` (the legacy
///     hardcoded default that beta-a's `build_phase_gates` already
///     applies for both phases).
#[must_use]
pub fn build_walk_config(
    team: &orchestrator_types::TeamTopology,
) -> orchestrator_types::lifecycle::WalkConfig {
    use orchestrator_types::lifecycle::{GatePolicy, NodeConfig, WalkConfig};
    use orchestrator_types::team::NodeClass;
    use orchestrator_types::NodeId;
    use std::collections::HashMap;

    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for edge in &team.message_graph {
        let from = NodeId(edge.from.name.0.clone());
        let to = NodeId(edge.to.name.0.clone());
        adjacency.entry(from).or_default().push(to);
    }

    let default_class = NodeClass("container_bound".to_string());
    let mut node_configs: HashMap<NodeId, NodeConfig> = HashMap::new();
    for role in &team.roles {
        let class = team
            .node_classes
            .get(&role.name)
            .cloned()
            .unwrap_or_else(|| default_class.clone());
        let expected_inbound: Vec<NodeId> = team
            .inbound_roles(role)
            .into_iter()
            .map(|r| NodeId(r.name.0.clone()))
            .collect();
        let policy = team
            .incoming(role)
            .iter()
            .find_map(|e| e.gate_policy.clone())
            .unwrap_or(GatePolicy::AllMustPass);
        node_configs.insert(
            NodeId(role.name.0.clone()),
            NodeConfig {
                class,
                expected_inbound,
                policy,
            },
        );
    }

    WalkConfig {
        adjacency,
        node_configs,
    }
}

/// Derive the topology root from a `WalkConfig` — the unique node whose
/// `expected_inbound` is empty. Returns `Err(Reason::TopologyInvalid)`
/// when no such node exists or when more than one does (a topology-data
/// bug that the projector flags by failing the brief rather than
/// silently routing through an arbitrary node).
pub fn derive_entry_node(
    walk_config: &orchestrator_types::lifecycle::WalkConfig,
) -> std::result::Result<orchestrator_types::NodeId, orchestrator_types::lifecycle::Reason> {
    let mut roots: Vec<&orchestrator_types::NodeId> = walk_config
        .node_configs
        .iter()
        .filter(|(_, cfg)| cfg.expected_inbound.is_empty())
        .map(|(id, _)| id)
        .collect();
    match roots.len() {
        0 => Err(orchestrator_types::lifecycle::Reason::TopologyInvalid {
            detail: "no entry vertex (every node has at least one expected_inbound)".to_string(),
        }),
        1 => Ok(roots.pop().expect("len==1 just checked").clone()),
        n => Err(orchestrator_types::lifecycle::Reason::TopologyInvalid {
            detail: format!("expected exactly one entry vertex, found {n}"),
        }),
    }
}
