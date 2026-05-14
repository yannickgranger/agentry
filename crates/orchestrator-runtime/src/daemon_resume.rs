//! Boot-time orphan scan: walks every non-terminal `agentry:brief:*:state`
//! key and either reattaches to its still-running container by re-spawning
//! the per-brief lifecycle driver task, or marks the brief
//! `Failed { DaemonRestartedDuringExecution }` when the named container is
//! gone. Closes the operational hole where any `orchestratord` restart
//! either silently orphaned every in-flight brief (#471) or — post-471a
//! but pre-471b — killed every alive in-flight brief on every redeploy.
//!
//! 471a delivered the conservative fail-only path. 471b adds the reattach
//! happy path: when the agent container is still alive, the daemon
//! re-spawns `lifecycle_driver::projector_task` for the brief and lets
//! the projector resume reading from the brief's trace stream cursor.

use crate::lifecycle::{EventSource, StateProjector};
use crate::{lifecycle_driver, Config, Error, Result};
use orchestrator_infra::redis_io;
use orchestrator_types::lifecycle::{BriefState, BriefStateRecord, Reason};
use orchestrator_types::RunData;
use orchestrator_types::{now, Brief, BriefId};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::sync::Arc;

/// Counts emitted at the end of one [`resume_orphans`] sweep.
///
/// `kept_alive` is the count of briefs whose container was alive at scan
/// time AND whose lifecycle driver was successfully re-spawned. A brief
/// whose container was alive but whose reattach setup failed (body
/// missing, team lookup failed, etc.) lands in `reattach_failed` instead
/// — its `:state` is rewritten to `Failed { DaemonRestartedDuringExecution }`
/// just like a dead-container brief, but the operator-visible counter is
/// distinct so a half-live recovery is not conflated with a clean
/// dead-on-arrival case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReport {
    /// Total non-terminal `:state` records the scan inspected (terminal
    /// records are filtered before counting).
    pub scanned: usize,
    /// Records transitioned to `Failed { DaemonRestartedDuringExecution }`
    /// because the named container was not alive.
    pub failed_dead: usize,
    /// Records whose container was alive and whose lifecycle driver was
    /// successfully re-spawned (the projector resumes reading from the
    /// brief's trace stream cursor).
    pub kept_alive: usize,
    /// Count of briefs whose container was alive at scan time but
    /// reattach failed (e.g., body deserialization, team lookup). These
    /// briefs are marked `Failed { DaemonRestartedDuringExecution }` and
    /// their containers are NOT killed (operator may want to inspect).
    pub reattach_failed: usize,
}

/// Walk every `agentry:brief:*:state` key. For each non-terminal record,
/// either reattach (live container, projector_task spawned) or mark the
/// brief `Failed { DaemonRestartedDuringExecution }` (dead container or
/// reattach setup failure). Returns the per-bucket counts as a
/// [`ResumeReport`].
///
/// The function never panics. Per-record failures (deserialize errors,
/// SET/XADD failures) are logged at WARN and the scan continues; only a
/// SCAN-cursor backend error short-circuits with `Err`.
pub async fn resume_orphans(
    conn: &mut ConnectionManager,
    event_source_factory: &Arc<dyn Fn(BriefId) -> Box<dyn EventSource + Send> + Send + Sync>,
    state_projector_factory: &Arc<dyn Fn(BriefId) -> Box<dyn StateProjector + Send> + Send + Sync>,
    cfg: &Config,
) -> Result<ResumeReport> {
    tracing::info!("resume scan begun");

    let keys = scan_state_keys(conn).await?;

    let mut report = ResumeReport {
        scanned: 0,
        failed_dead: 0,
        kept_alive: 0,
        reattach_failed: 0,
    };

    for key in keys {
        let raw: Option<String> = match conn.get(&key).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "resume: GET failed, skipping");
                continue;
            }
        };
        let Some(raw) = raw else { continue };
        let record: BriefStateRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "resume: malformed state record, skipping");
                continue;
            }
        };

        // Terminal records are skipped silently — never re-write or
        // duplicate state_log entries for already-Shipped/Failed briefs.
        if matches!(
            record.state,
            BriefState::Shipped | BriefState::Failed { .. }
        ) {
            continue;
        }

        report.scanned += 1;

        let brief_id = record.brief_id.clone();
        let agent_id = brief_state_agent_id(&record.state);

        // Decide whether to attempt a reattach.
        //
        // - `Authoring` reattaches when its recorded container is still
        //   alive (471b path). Without a live container we cannot resume
        //   in-flight work, so it falls through to failed_dead.
        // - `AwaitingCaptainDecision` reattaches unconditionally (487a):
        //   the brief is operator-gated, no agent container is in
        //   flight, and the projector_task only needs to consume the
        //   eventual `CaptainAccepted` / `CaptainRejected` event the
        //   operator pushes via `captain decide` — at which point the
        //   FSM transitions normally.
        // Post-#495-beta-b: all non-terminal states are `Walking`. The
        // reattach decision keys off `run_data`:
        //
        //  - `Coder { agent_id }` — probe `container_alive(agent_id)`;
        //    reattach iff the container is still up.
        //  - `OperatorDecision { .. }` — always reattach (operator-gated
        //    parking; container is dead by design, FSM driver consumes
        //    `CaptainAccepted` / `CaptainRejected` events from the
        //    operator's `captain decide` invocation). Pinned fix for
        //    v4 reviewer-claude blocker B8.
        //  - `PrTracking { .. }` — always reattach (the ci-watcher node
        //    is daemon-poll-driven, not container-bound after the
        //    initial spawn).
        //  - `None` / `Extension { .. }` — fall back to the node's
        //    declared class via the topology (container_bound nodes
        //    require a live container; operator_gated nodes always
        //    reattach).
        let should_reattach = match &record.state {
            BriefState::Walking { run_data, .. } => match run_data {
                RunData::Coder { agent_id } => container_alive(agent_id).await,
                RunData::OperatorDecision { .. } => true,
                RunData::PrTracking { .. } => true,
                RunData::None | RunData::Extension { .. } => true,
            },
            _ => false,
        };

        if should_reattach {
            match reattach_brief(
                &brief_id,
                event_source_factory,
                state_projector_factory,
                cfg,
                conn,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!(
                        brief = %brief_id.0,
                        agent = ?agent_id,
                        state = ?record.state,
                        "resume: reattached lifecycle driver",
                    );
                    report.kept_alive += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        brief = %brief_id.0,
                        agent = ?agent_id,
                        error = %e,
                        "resume: reattach failed; falling through to mark Failed",
                    );
                    if mark_failed(conn, &record).await {
                        report.reattach_failed += 1;
                    }
                    continue;
                }
            }
        }

        if mark_failed(conn, &record).await {
            report.failed_dead += 1;
        }
    }

    tracing::info!(
        scanned = report.scanned,
        failed_dead = report.failed_dead,
        kept_alive = report.kept_alive,
        reattach_failed = report.reattach_failed,
        "resume scan complete",
    );

    Ok(report)
}

/// Re-spawn the per-brief `lifecycle_driver::projector_task` for a brief
/// whose container is still alive on boot. Constructs the same
/// `EventSource` / `StateProjector` shape the original dispatch used and
/// hands them to `projector_task`; the projector resumes from the trace
/// stream cursor and observes any subsequent terminal event the
/// container produces.
///
/// **Limitation (v0 reattach).** This brief (471b) does NOT reattach the
/// role-chain side of the original dispatch (`handle_brief`'s
/// outbox-watching loop). The role chain's in-process state — polling
/// cursors, in-memory retry budgets, semaphores — is gone at restart,
/// and faithfully resurrecting it is out of scope here. The agent
/// container itself keeps producing trace events that the new projector
/// consumes: if the container completes normally the projector observes
/// the terminal event and writes the verdict via the existing
/// terminal-emit code path; if the container produces an error event
/// the FSM transitions to `Failed` via the existing universal handler.
/// The single observable loss is the role chain's ability to dispatch
/// the NEXT role in the chain (e.g., spawn the reviewer after the
/// coder ships) — post-reattach briefs DO complete the in-flight role's
/// work but do NOT auto-progress to the next role. A v2 reattach can
/// re-spawn the role chain via a follow-up brief; for v0 this is the
/// acceptable cost for not losing all in-flight work on every redeploy.
#[tracing::instrument(skip_all, fields(brief = %brief_id.0))]
async fn reattach_brief(
    brief_id: &BriefId,
    event_source_factory: &Arc<dyn Fn(BriefId) -> Box<dyn EventSource + Send> + Send + Sync>,
    state_projector_factory: &Arc<dyn Fn(BriefId) -> Box<dyn StateProjector + Send> + Send + Sync>,
    cfg: &Config,
    conn: &mut ConnectionManager,
) -> Result<()> {
    // cfg is threaded through for forward-compat with future reattach
    // policy (e.g., per-config wallclock fence on reattach age) — the v0
    // reattach does not consult it.
    let _ = cfg;

    let body_key = format!("agentry:brief:{}:body", brief_id.0);
    let raw: Option<String> = conn
        .get(&body_key)
        .await
        .map_err(|e| Error::Config(format!("reattach: GET {body_key}: {e}")))?;
    let raw = raw.ok_or_else(|| {
        Error::Config(format!(
            "reattach: missing brief body at {body_key}; cannot resolve topology"
        ))
    })?;
    let brief: Brief = serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("reattach: deserialize brief body: {e}")))?;

    let team = redis_io::fetch_team(conn, &brief.topology)
        .await
        .map_err(|e| Error::Config(format!("reattach: fetch_team: {e}")))?;
    let walk_config = lifecycle_driver::build_walk_config(&team);
    let entry_node = lifecycle_driver::derive_entry_node(&walk_config).map_err(|reason| {
        Error::Config(format!(
            "reattach: topology has no unique entry vertex: {reason:?}"
        ))
    })?;
    let walk_config = Arc::new(walk_config);
    let entry_node = Arc::new(entry_node);

    let event_source = (event_source_factory)(brief_id.clone());
    let state_projector = (state_projector_factory)(brief_id.clone());
    let verdict_conn = conn.clone();

    tokio::spawn(lifecycle_driver::projector_task(
        brief_id.clone(),
        event_source,
        state_projector,
        Some(verdict_conn),
        walk_config,
        entry_node,
    ));

    Ok(())
}

/// Write `Failed { DaemonRestartedDuringExecution }` for the given
/// non-terminal record. Returns `true` if the SET landed (the XADD to
/// `:state_log` is best-effort — its failure is logged but does not
/// flip the return value, mirroring 471a behaviour). Returns `false`
/// only on serialization or SET failure, in which case nothing was
/// written and the caller should NOT bump any counter for this record.
#[tracing::instrument(skip_all, fields(brief = %record.brief_id.0))]
async fn mark_failed(conn: &mut ConnectionManager, record: &BriefStateRecord) -> bool {
    let new_record = BriefStateRecord {
        brief_id: record.brief_id.clone(),
        state: BriefState::Failed {
            reason: Reason::DaemonRestartedDuringExecution,
        },
        parent_brief_id: record.parent_brief_id.clone(),
        composition_role: record.composition_role.clone(),
        at: now(),
    };

    let json = match serde_json::to_string(&new_record) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(brief = %record.brief_id.0, error = %e, "resume: serialize failed, skipping");
            return false;
        }
    };

    let state_key = format!("agentry:brief:{}:state", record.brief_id.0);
    let log_key = format!("agentry:brief:{}:state_log", record.brief_id.0);

    if let Err(e) = conn.set::<_, _, ()>(&state_key, json.as_str()).await {
        tracing::warn!(brief = %record.brief_id.0, error = %e, "resume: SET state failed, skipping");
        return false;
    }
    if let Err(e) = conn
        .xadd::<_, _, _, _, String>(&log_key, "*", &[("record", json.as_str())])
        .await
    {
        tracing::warn!(brief = %record.brief_id.0, error = %e, "resume: XADD state_log failed (state already updated)");
    }

    tracing::info!(
        brief = %record.brief_id.0,
        "resume: marked Failed {{ DaemonRestartedDuringExecution }}",
    );
    true
}

async fn scan_state_keys(conn: &mut ConnectionManager) -> Result<Vec<String>> {
    let mut keys: Vec<String> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("agentry:brief:*:state")
            .arg("COUNT")
            .arg(100)
            .query_async(conn)
            .await
            .map_err(Error::from)?;
        for key in batch {
            if key.ends_with(":state") {
                keys.push(key);
            }
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    Ok(keys)
}

/// Extract the `agent_id` named on a brief's current `BriefState`, when the
/// state variant pins one. Post-#495-beta-b only
/// `Walking { run_data: RunData::Coder { agent_id }, .. }` carries an
/// agent_id; every other Walking variant (PrTracking, OperatorDecision,
/// None, Extension) returns `None` and the caller decides what to do
/// with the missing target.
///
/// Public so `crate::cli_abort` can reuse the same accessor when the
/// per-brief abort path needs to best-effort `podman stop`/`kill` the
/// brief's container.
#[must_use]
pub fn brief_state_agent_id(state: &BriefState) -> Option<&str> {
    match state {
        BriefState::Walking {
            run_data: RunData::Coder { agent_id },
            ..
        } => Some(agent_id.as_str()),
        _ => None,
    }
}

async fn container_alive(agent_id: &str) -> bool {
    let name_filter = format!("name=agentry-{agent_id}");
    let output = tokio::process::Command::new("podman")
        .args([
            "ps",
            "--filter",
            name_filter.as_str(),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Ok(o) => {
            tracing::warn!(
                agent = %agent_id,
                status = ?o.status,
                stderr = %String::from_utf8_lossy(&o.stderr),
                "resume: podman ps failed, treating container as dead",
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                agent = %agent_id,
                error = %e,
                "resume: podman ps spawn failed, treating container as dead",
            );
            false
        }
    }
}
