//! Boot-time orphan scan: walks every non-terminal `agentry:brief:*:state`
//! key and marks the brief `Failed { DaemonRestartedDuringExecution }` when
//! its named container is no longer alive. Closes the operational hole
//! where any `orchestratord` restart silently orphans every in-flight
//! brief — see issue #471.
//!
//! This is the conservative fail-only path of the proposed fix. The
//! container-alive REATTACH happy path is deferred to a follow-up brief
//! 471b; until then, an alive container is left as-is and `kept_alive` in
//! the returned [`ResumeReport`] always stays at zero (the field is kept
//! for shape stability across the 471b change).

use crate::{Error, Result};
use orchestrator_types::lifecycle::{BriefState, BriefStateRecord, Reason};
use orchestrator_types::now;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

/// Counts emitted at the end of one [`resume_orphans`] sweep.
///
/// `kept_alive` is always zero in the 471a fail-only path; the field
/// exists so the report shape stays stable when 471b lands the reattach
/// branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReport {
    /// Total non-terminal `:state` records the scan inspected (terminal
    /// records are filtered before counting).
    pub scanned: usize,
    /// Records transitioned to `Failed { DaemonRestartedDuringExecution }`
    /// because the named container was not alive.
    pub failed_dead: usize,
    /// Records whose container was found alive — left untouched for the
    /// future 471b reattach. Always zero in the 471a path.
    pub kept_alive: usize,
}

/// Walk every `agentry:brief:*:state` key, fail any non-terminal record
/// whose container is no longer alive, and return a [`ResumeReport`].
///
/// The function never panics. Per-record failures (deserialize errors,
/// SET/XADD failures) are logged at WARN and the scan continues; only a
/// SCAN-cursor backend error short-circuits with `Err`.
pub async fn resume_orphans(conn: &mut ConnectionManager) -> Result<ResumeReport> {
    tracing::info!("resume scan begun");

    let keys = scan_state_keys(conn).await?;

    let mut report = ResumeReport {
        scanned: 0,
        failed_dead: 0,
        kept_alive: 0,
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

        // Without an agent_id we cannot probe a container — treat as
        // dead and fail conservatively. Past-Authoring states (Verifying,
        // Reviewing, Reworking, Shipping, Watching, Extension) fall in
        // this bucket today; 471b can refine once it has a per-role
        // agent inventory to consult.
        let alive = match agent_id {
            Some(id) => container_alive(id).await,
            None => false,
        };

        if alive {
            report.kept_alive += 1;
            continue;
        }

        let new_record = BriefStateRecord {
            brief_id: brief_id.clone(),
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
                tracing::warn!(brief = %brief_id.0, error = %e, "resume: serialize failed, skipping");
                continue;
            }
        };

        let state_key = format!("agentry:brief:{}:state", brief_id.0);
        let log_key = format!("agentry:brief:{}:state_log", brief_id.0);

        if let Err(e) = conn.set::<_, _, ()>(&state_key, json.as_str()).await {
            tracing::warn!(brief = %brief_id.0, error = %e, "resume: SET state failed, skipping");
            continue;
        }
        if let Err(e) = conn
            .xadd::<_, _, _, _, String>(&log_key, "*", &[("record", json.as_str())])
            .await
        {
            tracing::warn!(brief = %brief_id.0, error = %e, "resume: XADD state_log failed (state already updated)");
        }

        tracing::info!(
            brief = %brief_id.0,
            agent = ?agent_id,
            "resume: marked Failed {{ DaemonRestartedDuringExecution }}",
        );
        report.failed_dead += 1;
    }

    tracing::info!(
        scanned = report.scanned,
        failed_dead = report.failed_dead,
        kept_alive = report.kept_alive,
        "resume scan complete",
    );

    Ok(report)
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

fn brief_state_agent_id(state: &BriefState) -> Option<&str> {
    match state {
        BriefState::Authoring { agent_id, .. } => Some(agent_id.as_str()),
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
