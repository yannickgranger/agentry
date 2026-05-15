//! Daemon: XREAD loop on `agentry:briefs`, per-brief orchestration.
//!
//! The outer loop reads briefs off Redis and dispatches each to its own
//! `tokio::spawn`d task so multiple briefs run concurrently. Within a brief,
//! `handle_brief` walks the team's `message_graph` as a DAG: roles whose
//! upstream(s) have all shipped fire concurrently via `join_all`. Rework
//! rewinds to the single upstream named by `team.incoming(role).first()`,
//! resetting that upstream and its downstream sub-DAG to pending so they
//! re-fire once the upstream re-ships.

use crate::intake_validation::{self, IntakeError};
use crate::{
    daemon_resume,
    lifecycle::{read_brief_state, EventSource, RedisStateProjector, StateProjector},
    lifecycle_driver, permit as permit_mod, projector, reaper, redis_io,
    spawner::{PodmanSpawner, RoutedMessage, RunAgentCtx, Spawner, TeamContext},
    state,
    workspace::{self, BriefWorkspace},
    Error, Result,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use futures::future::join_all;
use orchestrator_types::lifecycle::{BriefState, BriefStateRecord, Reason};
use orchestrator_types::{
    apply_overrides, now, AgentRole, Brief, BriefId, Budget, EventKind, EventVerdict, NodeId,
    PermitOverrides, PermitScope, RoleRef, TargetRepo, TeamTopology, ToolAllowlist, Verdict,
    VerdictKind, VersionedRef, WorkPermit,
};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Pure inner: resolve the daemon's work-root directory from explicit inputs.
///
/// Precedence: explicit `AGENTRY_WORK_ROOT` value, then `$HOME/.local/share/agentry/work`,
/// then `/tmp/agentry-work`. Pulled out as a pure function so integration tests can
/// exercise the precedence rules without mutating process env.
pub fn default_work_root_inner(
    env_var: Option<String>,
    home: Option<String>,
) -> std::path::PathBuf {
    if let Some(s) = env_var {
        if !s.is_empty() {
            return std::path::PathBuf::from(s);
        }
    }
    if let Some(h) = home {
        if !h.is_empty() {
            return std::path::PathBuf::from(h).join(".local/share/agentry/work");
        }
    }
    std::path::PathBuf::from("/tmp/agentry-work")
}

/// Public wrapper: read `AGENTRY_WORK_ROOT` and `HOME` from the process env and
/// delegate to [`default_work_root_inner`].
pub fn default_work_root() -> std::path::PathBuf {
    let env_var = std::env::var("AGENTRY_WORK_ROOT").ok();
    let home = std::env::var("HOME").ok();
    default_work_root_inner(env_var, home)
}

/// Run the daemon loop forever using the given `Config`.
///
/// `event_source_factory` and `state_projector_factory` are invoked
/// once per dispatched brief; the resulting [`EventSource`] and
/// [`StateProjector`] are handed to a per-brief
/// `lifecycle_driver::projector_task` that runs in parallel with the
/// existing orchestrator role-chain (see L.3a / EPIC #246). Production
/// callers wire the Redis adapters from `crate::lifecycle`; tests can
/// inject in-memory adapters.
///
/// [`EventSource`]: crate::lifecycle::EventSource
/// [`StateProjector`]: crate::lifecycle::StateProjector
pub async fn run(
    cfg: &crate::Config,
    event_source_factory: Arc<dyn Fn(BriefId) -> Box<dyn EventSource + Send> + Send + Sync>,
    state_projector_factory: Arc<dyn Fn(BriefId) -> Box<dyn StateProjector + Send> + Send + Sync>,
) -> Result<()> {
    let mut conn = redis_io::connect(&cfg.redis.url).await?;
    tracing::info!(url = %cfg.redis.url.rsplit('@').next().unwrap_or("?"), "connected to Redis");

    // Boot-time backfill: sweep orphan auto/* branches left behind by prior
    // shipped briefs (pre-fix daemons did not delete the branch ref on
    // teardown). Failure must not block boot — a stale branch only matters
    // when its id collides with a future dispatch.
    match workspace::sweep_orphan_branches(&BriefWorkspace::root()).await {
        Ok(n) => tracing::info!(swept = n, "boot: orphan auto/* branch sweep complete"),
        Err(e) => tracing::warn!(error = %e, "boot: orphan auto/* branch sweep failed (non-fatal)"),
    }

    let state_path = std::env::var("AGENTRY_STATE_PATH").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{home}/.config/agentry/state.db")
    });
    let parent = std::path::Path::new(&state_path)
        .parent()
        .expect("state path has parent dir");
    std::fs::create_dir_all(parent).map_err(|e| Error::Config(format!("create state dir: {e}")))?;
    let state = std::sync::Arc::new(state::open_or_init(std::path::Path::new(&state_path))?);
    tracing::info!(path = %state_path, "agent state store ready");
    tokio::spawn(projector::run(state.clone(), conn.clone()));

    // Boot-time orphan scan (#471a): every non-terminal :state whose named
    // container is no longer alive is failed with
    // Reason::DaemonRestartedDuringExecution. A corrupt :state must not
    // prevent boot — log and continue so the daemon comes up.
    match redis_io::connect(&cfg.redis.url).await {
        Ok(mut resume_conn) => match daemon_resume::resume_orphans(
            &mut resume_conn,
            &event_source_factory,
            &state_projector_factory,
            cfg,
        )
        .await
        {
            Ok(report) => tracing::info!(
                scanned = report.scanned,
                failed_dead = report.failed_dead,
                kept_alive = report.kept_alive,
                reattach_failed = report.reattach_failed,
                "boot: daemon resume scan complete",
            ),
            Err(e) => tracing::warn!(error = %e, "boot: daemon resume scan failed (non-fatal)"),
        },
        Err(e) => tracing::warn!(error = %e, "boot: daemon resume connect failed (non-fatal)"),
    }

    // Wall-clock reaper (L.5 of EPIC #246): scans every 30s for briefs
    // stuck in non-terminal state past their `budget.max_wall_seconds`,
    // pushes `BriefEvent::BudgetExhausted` to the trace stream so the
    // per-brief lifecycle FSM transitions to terminal Failed, and
    // best-effort `podman kill`s the orphan containers. Closes the
    // wall-clock-no-Failed orphan class (Cases 2/3/4 in
    // `docs/forensics/orphan_pattern.md`).
    let reaper_inventory = reaper::RedisInventory::new(conn.clone());
    let reaper_sink = reaper::RedisReaperSink::new(conn.clone());
    tokio::spawn(reaper::run(
        reaper_inventory,
        reaper_sink,
        reaper::DEFAULT_WALL_CLOCK_SECONDS,
        std::time::Duration::from_secs(reaper::REAPER_INTERVAL_SECONDS),
    ));
    match std::env::var("XAI_API_KEY") {
        Ok(key) if !key.is_empty() => {
            let watchdog_cfg = crate::watchdog::Watchdog::new_default(key);
            tokio::spawn(crate::watchdog::run(
                state.clone(),
                conn.clone(),
                watchdog_cfg,
            ));
        }
        _ => {
            tracing::info!("XAI_API_KEY not set; watchdog dormant — set it in orchestratord env to enable per-agent diagnostics");
        }
    }

    // Load signing key. Fail loudly if missing.
    let key_path = &cfg.signing.key_path;
    if !key_path.exists() {
        return Err(Error::Config(format!(
            "signing key not found at {}. Run `orchestrator key-gen` first.",
            key_path.display()
        )));
    }
    let signing_key = Arc::new(permit_mod::load_signing_key(key_path)?);
    let verifying_key = Arc::new(signing_key.verifying_key());
    tracing::info!(key = %key_path.display(), "signing key loaded");

    let spawner = PodmanSpawner::new();
    let mut last_id = "$".to_string(); // only new briefs
    let mut project_semaphores: HashMap<String, Arc<Semaphore>> = HashMap::new();

    loop {
        match redis_io::read_next_brief(&mut conn, &last_id, 5_000).await {
            Ok(Some((sid, brief))) => {
                last_id = sid;
                tracing::info!(
                    brief = %brief.id,
                    kind = ?brief.kind,
                    contract_present = brief.contract.is_some(),
                    assertion_count = brief.contract.as_ref().map(|c| c.assertions.len()).unwrap_or(0),
                    requires_contract = brief.kind.map(|k| k.requires_contract()).unwrap_or(false),
                    "received brief",
                );
                if brief.kind.map(|k| k.requires_contract()).unwrap_or(false)
                    && brief.contract.is_none()
                {
                    tracing::warn!(
                        brief = %brief.id,
                        kind = ?brief.kind,
                        "non-trivial brief kind missing contract — log-only observation; future slice (B6) will reject at intake",
                    );
                }
                // Brief 1b — pre-mint intake gates. Run BEFORE clone, slug
                // derivation, or permit mint. Two failures land here:
                //   1. payload.target_repo missing or rejected by
                //      `TargetRepo::from_str` charset validation
                //      (closes G3: token-fragment injection).
                //   2. parsed owner not in cfg.forge.allowed_owners
                //      (defense-in-depth pre-permit gate; permit-broker
                //      `forge:write` enforcement remains downstream).
                let target_repo = match brief.target_repo() {
                    Some(t) => t,
                    None => {
                        emit_brief_rejected(
                            &mut conn,
                            &brief,
                            &IntakeError::MissingTargetRepo,
                            None,
                        )
                        .await;
                        continue;
                    }
                };
                if !cfg
                    .forge
                    .allowed_owners
                    .iter()
                    .any(|o| o == target_repo.owner())
                {
                    let err = IntakeError::OwnerNotAllowed {
                        owner: target_repo.owner().to_string(),
                    };
                    emit_brief_rejected(&mut conn, &brief, &err, Some(&target_repo)).await;
                    continue;
                }
                // B6b — anchor validation. If the brief carries a contract,
                // resolve every assertion's anchor against the local cfdb
                // keyspace and `specs/concepts/`. Any unresolved anchor
                // produces a Failed verdict at intake; the brief is not
                // spawned. A brief without a contract is unaffected (the
                // B3 WARN above remains log-only — this slice does not
                // flip it to a reject).
                if let Some(contract) = brief.contract.as_ref() {
                    let assertion_count = contract.assertions.len();
                    let workspace_root = default_work_root();
                    // F1d — populate per-target keyspace before resolution. ensure_target_extracted
                    // is idempotent (cache hit on (slug + head_sha)). Cache miss clones target_repo
                    // + runs cfdb extract + copies specs. Failure to extract is logged but does NOT
                    // abort intake — anchor resolution will simply return NotFound for affected
                    // anchors and the existing intake-reject path handles it.
                    let target_repo_str = target_repo.to_string();
                    let head_sha = brief
                        .payload
                        .get("base_branch")
                        .and_then(|v| v.as_str())
                        .unwrap_or("develop")
                        .to_string();
                    let forge_host = cfg
                        .forge
                        .default_host
                        .as_deref()
                        .unwrap_or("agency.lab:3000");
                    let clone_url = target_repo.clone_url(forge_host);
                    let extract_req = intake_validation::EnsureExtractedRequest {
                        target_repo: target_repo_str.clone(),
                        head_sha: head_sha.clone(),
                        clone_url,
                        work_root: workspace_root.clone(),
                    };
                    let extract_outcome = tokio::task::spawn_blocking(move || {
                        intake_validation::ensure_target_extracted(&extract_req)
                    })
                    .await
                    .map_err(|e| {
                        tracing::warn!(brief = %brief.id, error = %e, "ensure_target_extracted join failed");
                    });
                    match extract_outcome {
                        Ok(intake_validation::EnsureExtractedOutcome::CacheHit) => {
                            tracing::debug!(brief = %brief.id, target_repo = %target_repo_str, "ensure_target_extracted: cache hit");
                        }
                        Ok(intake_validation::EnsureExtractedOutcome::Extracted { items }) => {
                            tracing::info!(brief = %brief.id, target_repo = %target_repo_str, items = items, "ensure_target_extracted: extracted");
                        }
                        Ok(intake_validation::EnsureExtractedOutcome::Failed { reason }) => {
                            tracing::warn!(brief = %brief.id, target_repo = %target_repo_str, reason = %reason, "ensure_target_extracted: failed (degraded; resolution may NotFound)");
                        }
                        Err(_) => {
                            // join error already logged
                        }
                    }
                    let failures = match intake_validation::validate_brief_contract_for_target(
                        &brief,
                        &workspace_root,
                    ) {
                        Ok(f) => f,
                        Err(err) => {
                            // Should not happen — the gate above already
                            // checked target_repo parses — but stay defensive.
                            emit_brief_rejected(&mut conn, &brief, &err, Some(&target_repo)).await;
                            continue;
                        }
                    };
                    if failures.is_empty() {
                        tracing::info!(
                            brief = %brief.id,
                            assertions_resolved = assertion_count,
                            "intake: all contract anchors resolved",
                        );
                    } else {
                        let detail = format!(
                            "intake-reject: anchor unresolved — {} of {} assertions failed: {}",
                            failures.len(),
                            assertion_count,
                            failures
                                .iter()
                                .map(|(id, why)| format!("{id}={why}"))
                                .collect::<Vec<_>>()
                                .join("; ")
                        );
                        let verdict =
                            Verdict::new(brief.id.clone(), VerdictKind::Failed).with_reason(detail);
                        if let Err(e) = redis_io::append_verdict(&mut conn, &verdict).await {
                            tracing::error!(
                                brief = %brief.id,
                                error = %e,
                                "intake-reject: failed to append Failed verdict to verdicts stream",
                            );
                        }
                        tracing::warn!(
                            brief = %brief.id,
                            failures = ?failures,
                            "intake-reject: contract anchors unresolved — brief will not be spawned",
                        );
                        continue;
                    }
                }
                // Defensive backfill of agentry:brief:<id>:body so the
                // dashboard's SMEMBERS+MGET render path doesn't depend
                // on intake going through `submit_brief` (raw XADD,
                // operator tooling, replay/recovery all bypass it).
                // Idempotent overwrite of submit_brief's pre-write.
                if let Err(e) = backfill_body_key(&mut conn, &brief).await {
                    tracing::warn!(brief = %brief.id, error = %e, "body key backfill failed");
                }
                let conn_clone = conn.clone();
                let signing_clone = signing_key.clone();
                let verifying_clone = verifying_key.clone();
                let spawner_clone = spawner.clone();
                let cfg_clone = cfg.clone();
                let brief_id = brief.id.clone();
                let slug = brief.project.as_deref().unwrap_or("_global").to_string();
                let cap: u32 = if let Some(s) = brief.project.as_deref() {
                    match redis_io::fetch_project(&mut conn, s).await {
                        Ok(p) => p.max_concurrent_briefs.unwrap_or(cfg.max_concurrent_briefs),
                        Err(Error::NotFound { .. }) => cfg.max_concurrent_briefs,
                        Err(e) => {
                            tracing::warn!(brief = %brief.id, error = %e, "fetch_project failed; using global cap");
                            cfg.max_concurrent_briefs
                        }
                    }
                } else {
                    cfg.max_concurrent_briefs
                };
                let sem = project_semaphores
                    .entry(slug.clone())
                    .or_insert_with(|| Arc::new(Semaphore::new(cap as usize)))
                    .clone();
                let started = std::time::Instant::now();
                let permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(brief = %brief.id, error = %e, "semaphore closed; skipping brief");
                        continue;
                    }
                };
                let waited = started.elapsed();
                if waited > std::time::Duration::from_secs(1) {
                    tracing::info!(brief = %brief.id, project = %slug, cap = cap, waited_ms = waited.as_millis() as u64, "brief waited for project concurrency slot");
                }
                let event_source_factory_clone = event_source_factory.clone();
                let state_projector_factory_clone = state_projector_factory.clone();
                let conn_for_verdict_emit = conn.clone();
                tokio::spawn(async move {
                    let _permit = permit; // released on drop
                    let mut conn_for_brief = conn_clone;

                    // Project the team topology into the FSM's
                    // per-phase gate config (verifier/reviewer fan-in
                    // expected_roles, policy hardcoded AllMustPass for
                    // Phase 1). The projector_task threads this through
                    // every `handle()` call so Verifying/Reviewing
                    // construct with the gate config available;
                    // 396b-3 will swap the serial transitions for
                    // walk_config: built from team topology, threaded to
                    // the FSM projector for every handle() call. entry_node
                    // is the unique topology root (verified once per
                    // brief); if the topology is malformed the brief
                    // fails terminally with Reason::TopologyInvalid
                    // before any role spawns.
                    let (walk_config, entry_node) = match redis_io::fetch_team(
                        &mut conn_for_brief,
                        &brief.topology,
                    )
                    .await
                    {
                        Ok(team) => {
                            let wc = lifecycle_driver::build_walk_config(&team);
                            match lifecycle_driver::derive_entry_node(&wc) {
                                Ok(en) => (Arc::new(wc), Arc::new(en)),
                                Err(reason) => {
                                    tracing::error!(brief = %brief_id, ?reason, "topology missing unique entry vertex; skipping brief");
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(brief = %brief_id, error = %e, "fetch_team failed; skipping brief");
                            return;
                        }
                    };

                    // FSM projector: drives the brief lifecycle and is
                    // the sole writer to `agentry:verdicts`. The role
                    // chain below produces the trace events the
                    // projector consumes.
                    let event_source = (event_source_factory_clone)(brief_id.clone());
                    let state_projector = (state_projector_factory_clone)(brief_id.clone());
                    let projector_handle = tokio::spawn(lifecycle_driver::projector_task(
                        brief_id.clone(),
                        event_source,
                        state_projector,
                        Some(conn_for_verdict_emit),
                        walk_config,
                        entry_node,
                    ));

                    let outcome = handle_brief(
                        &mut conn_for_brief,
                        &spawner_clone,
                        &brief,
                        &cfg_clone,
                        &signing_clone,
                        &verifying_clone,
                    )
                    .await;

                    // The projector task tails the brief's trace stream;
                    // when handle_brief returns, the agents have all done
                    // their final XADDs but the projector may still be
                    // catching up. Detach the join handle — the task
                    // self-terminates on terminal-state transition.
                    drop(projector_handle);

                    match outcome {
                        Ok(brief_kind) => {
                            if let Err(e) =
                                dol_on_brief_terminal(&mut conn_for_brief, &brief, &brief_kind)
                                    .await
                            {
                                tracing::error!(brief = %brief_id, error = %e, "DOL hook failed");
                            }
                        }
                        Err(e) => {
                            tracing::error!(brief = %brief_id, error = %e, "brief handling failed");
                            let abort_event =
                                orchestrator_types::lifecycle::BriefEvent::AbortRequested {
                                    actor: "daemon".into(),
                                    message: format!("handler error: {e}"),
                                };
                            let payload = serde_json::to_value(&abort_event)
                                .unwrap_or_else(|_| serde_json::json!({}));
                            let event = orchestrator_types::Event::new(
                                orchestrator_types::EventKind::Event { payload },
                            );
                            if let Err(emit_err) = redis_io::append_trace(
                                &mut conn_for_brief,
                                &brief_id,
                                "daemon",
                                &event,
                            )
                            .await
                            {
                                tracing::warn!(
                                    brief = %brief_id,
                                    error = %emit_err,
                                    "append_trace AbortRequested failed"
                                );
                            }
                            dol_on_brief_terminal(
                                &mut conn_for_brief,
                                &brief,
                                &VerdictKind::Failed,
                            )
                            .await
                            .ok();
                        }
                    }
                });
            }
            Ok(None) => {
                // Block timeout with no entries; loop.
            }
            Err(e) => {
                tracing::error!(error = %e, "XREAD failed; backing off");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

/// Setup-phase bundle: one entry per role about to fire in the current batch.
/// Constructed serially before the concurrent fan-out.
struct RoleRun {
    role_ref: RoleRef,
    role: AgentRole,
    permit: WorkPermit,
    team_ctx: TeamContext,
}

/// Scan a brief's trace stream and group routed messages by their
/// destination role. The trace is the canonical source of routed
/// messages — `spawner::run_agent` appends every `EventKind::Message`
/// to `agentry:brief:{id}:trace` (spawner.rs:489). The in-process
/// `all_messages` accumulator in `handle_brief` is a redundant copy
/// that this helper can rebuild.
///
/// Role-name memoization mirrors `lifecycle_ports::translate_trace_entry`
/// (lifecycle_ports.rs:90+): every `spawned` event payload carries
/// `role_name`; subsequent events from the same `agent_id` inherit
/// that mapping. The returned `RoutedMessage.from` is the source
/// role's name, matching the in-process write at spawner.rs:478-484.
///
/// v2 finale (#539 phase 4) probe helper — at this commit, called
/// once per outer-loop iteration and used to log divergence vs the
/// in-process `all_messages` slice. Later phase switches the
/// authoritative source to this helper and deletes `all_messages`.
///
/// Trace read errors return an empty map with a WARN log — a
/// transient Redis hiccup must not crash a brief in flight; the
/// caller falls back to the in-process slice. The XRANGE count is
/// bounded at 4096 entries per scan, which exceeds any current
/// brief's trace length by an order of magnitude.
/// Pure trace→inbox reconstruction. Given trace entries as
/// `(agent_id, Event)` pairs in stream order, memoize
/// `agent_id → role_name` from `spawned` event payloads and group
/// `EventKind::Message` events by destination role.
///
/// This is the #539 phase-5 fence. It must reproduce **exactly** what
/// the in-process `all_messages` accumulator plus
/// `.filter(|m| m.to == role).cloned()` would have produced — the
/// deletion of `all_messages` in a later slice is only safe because
/// this reconstruction is proven equivalent by
/// `tests/messages_by_role.rs`.
///
/// `from` resolution has two paths:
///
/// 1. **Agent messages** — every agent emits a `spawned` event carrying
///    its `role_name` BEFORE any `Message` it sends (guaranteed by
///    `spawner::run_agent`, which appends the spawn trace entry at
///    container start and only later forwards agent stdout). The trace
///    `agent` field is the agent's UUID; the memo maps UUID → role
///    name, recovering the same value the in-process write at
///    spawner.rs:478-484 stamped (`RoutedMessage.from = role.name.0`).
/// 2. **Daemon-attributed messages** — `handle_brief` synthesizes the
///    rework findings message and appends it to the trace with the
///    `agent` field set to the *source role's name* (not a UUID),
///    because no agent process emitted it. Such an `agent_id` is never
///    in the spawned memo, so the fallback uses it directly as `from`.
///    This reproduces the pre-5c in-process write
///    `RoutedMessage { from: from_ref.name.0, .. }` exactly.
///
/// The two paths are unambiguous: agent UUIDs never collide with role
/// names (UUIDs are `agt_<hex>`; role names are `coder-claude-agentry`
/// etc.), and the spawned-first invariant guarantees a real agent
/// message always hits the memo. `tests/messages_by_role.rs` pins both.
///
/// Pure (no I/O) so it is unit-testable without a live Redis stream.
/// `pub` for the integration test; graph-specs gates only
/// `pub struct/enum/trait/type`, so a `pub fn` needs no spec entry
/// (same rationale as `default_work_root`).
pub fn group_messages_by_role(
    entries: &[(String, orchestrator_types::Event)],
) -> HashMap<String, Vec<RoutedMessage>> {
    let mut role_by_agent: HashMap<String, String> = HashMap::new();
    let mut out: HashMap<String, Vec<RoutedMessage>> = HashMap::new();
    for (agent_id, ev) in entries {
        match &ev.kind {
            EventKind::Event { payload } => {
                if payload.get("agent_event").and_then(|v| v.as_str()) == Some("spawned") {
                    if let Some(rn) = payload.get("role_name").and_then(|v| v.as_str()) {
                        role_by_agent.insert(agent_id.clone(), rn.to_string());
                    }
                }
            }
            EventKind::Message { to, payload } => {
                // Memo hit → agent message (from = role name).
                // Memo miss → daemon-attributed message; the daemon
                // wrote the source role's name as `agent_id`, so use
                // it directly as `from`.
                let from = role_by_agent
                    .get(agent_id)
                    .cloned()
                    .unwrap_or_else(|| agent_id.clone());
                out.entry(to.clone()).or_default().push(RoutedMessage {
                    from,
                    to: to.clone(),
                    payload: payload.clone(),
                    at: ev.at,
                });
            }
            _ => {}
        }
    }
    out
}

/// Resolve the permit-narrowing override for `target_role` from its
/// trace-reconstructed inbound messages — the #539 phase-6 replacement
/// for the in-process `overrides_for` HashMap.
///
/// Pre-5c the daemon walked each completed role's outbox and, for
/// every edge carrying `permit_overrides_from = Some(key)`, stored
/// `msg.payload[key]` deserialized as `PermitOverrides` keyed by the
/// edge's `to` role (`HashMap::insert`, last-write-wins). This
/// reconstructs the same fact from the trace: among the messages
/// addressed to `target_role` (in trace/arrival order, as
/// `group_messages_by_role` yields them), find each edge
/// `msg.from → target_role` whose `permit_overrides_from` names a key
/// present in `msg.payload`, deserialize it, and let the last one win
/// — identical to the old `HashMap::insert` order semantics. A payload
/// that fails to deserialize is logged WARN and skipped (same
/// tolerance as the deleted in-process loop).
///
/// Pure (no I/O); `pub` for the integration test. graph-specs gates
/// only `pub struct/enum/trait/type`, so a `pub fn` needs no spec
/// entry (same rationale as `default_work_root`).
pub fn overrides_from_messages(
    team: &TeamTopology,
    target_role: &str,
    inbound: &[RoutedMessage],
) -> Option<PermitOverrides> {
    let mut resolved: Option<PermitOverrides> = None;
    for msg in inbound {
        for edge in team.message_graph.iter().filter(|e| {
            e.from.name.0 == msg.from
                && e.to.name.0 == target_role
                && e.permit_overrides_from.is_some()
        }) {
            let key = edge
                .permit_overrides_from
                .as_ref()
                .expect("filtered to is_some");
            if let Some(value) = msg.payload.get(key) {
                match serde_json::from_value::<PermitOverrides>(value.clone()) {
                    Ok(po) => resolved = Some(po),
                    Err(e) => {
                        tracing::warn!(
                            from = %msg.from,
                            to = %target_role,
                            key = %key,
                            error = %e,
                            "trace override message had override key but payload didn't deserialize"
                        );
                    }
                }
            }
        }
    }
    resolved
}

/// Scan a brief's trace stream (XRANGE, bounded 4096) and return the
/// per-role inbox map via [`group_messages_by_role`]. Trace read
/// errors return an empty map with a WARN log — a transient Redis
/// hiccup must not crash a brief in flight; the caller falls back to
/// the in-process slice. See [`group_messages_by_role`] for the
/// reconstruction semantics and the phase-5 equivalence invariant.
async fn messages_by_role_from_trace(
    conn: &mut ConnectionManager,
    brief_id: &BriefId,
) -> HashMap<String, Vec<RoutedMessage>> {
    let stream = format!("agentry:brief:{}:trace", brief_id.0);
    let reply: redis::streams::StreamRangeReply =
        match conn.xrange_count(&stream, "-", "+", 4096_usize).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    brief = %brief_id,
                    error = %e,
                    "messages_by_role_from_trace xrange failed; returning empty map"
                );
                return HashMap::new();
            }
        };
    let mut entries: Vec<(String, orchestrator_types::Event)> = Vec::with_capacity(reply.ids.len());
    for entry in reply.ids {
        let agent_id = entry
            .map
            .get("agent")
            .and_then(orchestrator_infra::redis_io::redis_value_as_str)
            .unwrap_or_default();
        let body = entry
            .map
            .get("event")
            .and_then(orchestrator_infra::redis_io::redis_value_as_str)
            .unwrap_or_default();
        let Ok(ev) = serde_json::from_str::<orchestrator_types::Event>(&body) else {
            continue;
        };
        entries.push((agent_id, ev));
    }
    group_messages_by_role(&entries)
}

/// FSM-side view of a brief's progression at iteration top of the
/// team-orchestration loop in `handle_brief`. Returns
/// `Some((shipped_set, fsm_retry_attempt))` while the FSM is in a
/// non-terminal state (`Walking` or transient `Submitted` / read
/// error), or `None` when the FSM has reached `Shipped`/`Failed` and
/// the caller MUST exit the outer loop.
///
/// - `shipped_set` — roles `Shipped` in the FSM's `Walking.evidence`.
///   The FSM is now the sole authority (phase 7b): the in-process
///   `shipped_roles` accumulator + the union are gone. The
///   `await_fsm_settled` barrier at the end of each loop iteration
///   guarantees the driver has consumed the prior batch before this
///   read, so there is no lag window to cover.
///
/// - `fsm_retry_attempt` — `Walking.retry.attempt` when in `Walking`,
///   `None` otherwise (`Submitted` or transient error). The caller
///   uses it directly as the rework-budget counter.
///
/// Terminal discriminator (issue #562): the pre-fix `_ => (empty,
/// None)` arm collapsed `Submitted` (transient, entry-node should
/// fire) and terminal `Shipped`/`Failed` (FSM is done, the outer
/// loop should stop) into the same empty-set return. The caller
/// could not tell them apart, so on terminal `Shipped` it kept
/// computing a ready-set from an empty `shipped_set` and respawned
/// the entry role into a workspace where the brief's commit had
/// already been merged — the respawn returned `no_changes`, and the
/// terminal `:state` was atomically rewritten from `Shipped` to
/// `Failed`. The actual work landed on `develop` (the shipper had
/// already opened + merged the auto/ PR before the post-terminal
/// respawn fired) but the dashboard's terminal-verdict view was
/// corrupted. `None` is now an explicit "stop the loop" signal.
///
/// Read errors yield `Some((empty, None))` with a WARN — a transient
/// Redis hiccup must not crash a brief; the empty-shipped-set path
/// fires the entry node, which the FSM's own state will correct on
/// the following settled iteration.
///
/// v2 finale (#539) helper.
async fn read_walking_view(
    conn: &mut ConnectionManager,
    brief_id: &BriefId,
    team: &TeamTopology,
) -> Option<(HashSet<RoleRef>, Option<u32>)> {
    let fsm_state = match read_brief_state(conn, brief_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                brief = %brief_id,
                error = %e,
                "FSM state read failed; treating shipped-set as empty for this iteration"
            );
            None
        }
    };
    match fsm_state.as_ref().map(|r| &r.state) {
        Some(BriefState::Walking {
            evidence, retry, ..
        }) => {
            // NodeId in `Walking.evidence` is constructed from
            // `role.name.0` by the translator (lifecycle_ports.rs:165
            // and the `Some(_)` arm at :185); the inverse lookup is
            // name-keyed via team.roles.
            let fsm: HashSet<RoleRef> = team
                .roles
                .iter()
                .filter(|r| {
                    evidence
                        .get(&NodeId(r.name.0.clone()))
                        .map(|v| matches!(v, EventVerdict::Shipped))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            Some((fsm, Some(retry.attempt)))
        }
        Some(BriefState::Shipped) | Some(BriefState::Failed { .. }) => None,
        _ => Some((HashSet::new(), None)),
    }
}

/// Poll interval and total budget for [`await_fsm_settled`]. The
/// pre-fix budget of 50×100ms = 5s was empirically too short for the
/// projector's observed settle latency on `agentry-self-host-v0`
/// briefs (orchestratord log on every trivial-doc brief 2026-05-15
/// showed `await_fsm_settled timed out after ~5s` between coder
/// `Shipped` and a respawn of the same role 6+ minutes later, which
/// the second-run coder correctly emitted `no_changes` on — discarding
/// the first run's commit). The budget is raised here to comfortably
/// exceed observed settle times; timeout is now a hard failure of the
/// brief (see [`await_fsm_settled`]).
const AWAIT_FSM_SETTLED_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
const AWAIT_FSM_SETTLED_BUDGET_TICKS: u32 = 3000; // 200ms × 3000 = 600s (10min)

/// Bounded poll until the FSM driver has settled w.r.t. the
/// just-completed batch (see [`fsm_settled`] for the per-outcome
/// settle conditions). The team-orchestration loop calls this at the
/// end of each iteration so the next iteration's `read_walking_view`
/// reads FSM state that already reflects this batch — replacing the
/// deleted in-process `shipped_roles`/`reworks_used` union.
///
/// `state_projector_cursor` cannot gate this (synthetic `step-N`, not
/// a trace id — lifecycle_driver.rs:45-47), so the barrier is
/// content-based via `fsm_settled`. Bounded by
/// [`AWAIT_FSM_SETTLED_BUDGET_TICKS`] × [`AWAIT_FSM_SETTLED_POLL_INTERVAL`]
/// = ~10 minutes total. Returns `true` when the FSM has settled,
/// `false` on timeout.
///
/// On timeout the caller MUST fail the brief: proceeding to the next
/// `read_walking_view` with stale `Walking.evidence` causes the loop
/// to respawn an already-`Shipped` role (the FSM has not yet recorded
/// the verdict), and the second run correctly judges `no_changes` and
/// fails the team — discarding the first run's valid commit. That
/// pre-fix behavior was the root cause of every trivial-doc brief
/// failure on `agentry-self-host-v0` after #559 landed.
async fn await_fsm_settled(
    conn: &mut ConnectionManager,
    brief_id: &BriefId,
    started_attempt: u32,
    expect_shipped: &[RoleRef],
    had_rework: bool,
) -> bool {
    for _ in 0..AWAIT_FSM_SETTLED_BUDGET_TICKS {
        let state = match read_brief_state(conn, brief_id).await {
            Ok(Some(rec)) => rec.state,
            Ok(None) => BriefState::Submitted,
            Err(e) => {
                tracing::warn!(
                    brief = %brief_id,
                    error = %e,
                    "await_fsm_settled state read failed; retrying"
                );
                tokio::time::sleep(AWAIT_FSM_SETTLED_POLL_INTERVAL).await;
                continue;
            }
        };
        if fsm_settled(&state, started_attempt, expect_shipped, had_rework) {
            return true;
        }
        tokio::time::sleep(AWAIT_FSM_SETTLED_POLL_INTERVAL).await;
    }
    tracing::warn!(
        brief = %brief_id,
        had_rework,
        "await_fsm_settled timed out after ~10min; FSM driver did not settle — caller will fail brief"
    );
    false
}

/// Has the FSM driver "settled" with respect to a just-completed
/// batch? The team-orchestration loop and `lifecycle_driver::
/// projector_task` are independent async tasks: the loop dispatches a
/// batch and observes agent outcomes, while the driver independently
/// consumes the trace and drives the FSM. Before the loop recomputes
/// its ready-set from FSM `Walking.evidence` (phase 7b, FSM-only),
/// it must wait until the driver has consumed this batch's terminal
/// events — otherwise it reads stale evidence.
///
/// `state_projector_cursor` cannot serve as the barrier: it is a
/// synthetic `step-N` counter (lifecycle_driver.rs:45-47,76-77 — the
/// real trace-id cursor is L.3b-deferred), uncorrelatable with trace
/// entry ids. So the barrier is **content-based** and must branch on
/// the batch's outcome, which `handle_brief` knows:
///
/// - Terminal (`Shipped`/`Failed`) → the FSM has decided; settled.
/// - Rework batch (`had_rework`) → the FSM resets evidence to empty
///   and bumps `retry.attempt` on the `ReworkNeeded`/`Failed`
///   `RoleDone` (lifecycle.rs:516-519). Emptied evidence is
///   indistinguishable from "not yet consumed" by evidence alone, so
///   the settle signal is `retry.attempt > started_attempt`.
/// - Advance batch → settled when every role in `expect_shipped` is
///   `Shipped` in `evidence`.
/// - `Submitted` → never settled (the FSM has not started the walk).
///
/// Pure (no I/O) so the decision is unit-testable without a live
/// Redis stream; the async poll wrapper (phase 7b) calls this each
/// tick. `pub fn` → no graph-specs entry (only `pub struct/enum/
/// trait/type` are gated, per specs/dialect.md — same as
/// `default_work_root` / `group_messages_by_role`).
#[must_use]
pub fn fsm_settled(
    state: &BriefState,
    started_attempt: u32,
    expect_shipped: &[RoleRef],
    had_rework: bool,
) -> bool {
    match state {
        BriefState::Shipped | BriefState::Failed { .. } => true,
        BriefState::Submitted => false,
        BriefState::Walking {
            evidence, retry, ..
        } => {
            if had_rework {
                retry.attempt > started_attempt
            } else {
                expect_shipped.iter().all(|r| {
                    evidence
                        .get(&NodeId(r.name.0.clone()))
                        .map(|v| matches!(v, EventVerdict::Shipped))
                        .unwrap_or(false)
                })
            }
        }
    }
}

/// Handle a single brief end-to-end via DAG walk. Returns the brief's
/// terminal-verdict kind (Shipped or Failed) so the caller can drive the DOL
/// composer.
async fn handle_brief(
    conn: &mut ConnectionManager,
    spawner: &impl Spawner,
    brief: &Brief,
    cfg: &crate::Config,
    signing_key: &SigningKey,
    verifying_key: &VerifyingKey,
) -> Result<VerdictKind> {
    let team = redis_io::fetch_team(conn, &brief.topology).await?;

    // Slice I/2b — fetch `.agentry/profile.toml` from target_repo via the
    // forge contents API. Slice I/2c threads the resolved profile through
    // to the spawner so `profile.{coder,reviewer}.tool_packs` augment the
    // matching role's `tool_packs` at spawn time. Fetch errors are NOT
    // fatal — a missing or unreachable profile downgrades to "use
    // defaults" and the brief proceeds.
    let resolved_profile = fetch_brief_profile(brief, cfg).await;

    // Dispatch-time validation hook: catch malformed topologies before
    // spawning anything. The validator catches `roles.is_empty()` via the
    // Type check, but the explicit guard below is kept as defense-in-depth.
    let registered_roles = redis_io::list_roles(conn).await?;
    let violations = crate::team_validator::validate(&team, &registered_roles);
    if !violations.is_empty() {
        let payload = serde_json::json!({
            "msg": "team_validation_failed",
            "team": team.name.0,
            "version": team.version,
            "violations": violations
                .iter()
                .map(|v| serde_json::json!({
                    "path": v.path,
                    "kind": format!("{:?}", v.kind),
                    "detail": v.detail,
                }))
                .collect::<Vec<_>>(),
        });
        let event =
            orchestrator_types::Event::new(orchestrator_types::EventKind::Event { payload });
        if let Err(e) = redis_io::append_trace(conn, &brief.id, "daemon", &event).await {
            tracing::warn!(brief = %brief.id, error = %e, "append_trace for team_validation_failed failed");
        }
        return Err(Error::Config(format!(
            "team {} v{} failed validation: {} violation(s)",
            team.name.0,
            team.version,
            violations.len()
        )));
    }

    if team.roles.is_empty() {
        return Err(Error::Config(format!("team {} has no roles", team.name.0)));
    }

    // #539 phase 5c: the in-process `all_messages` accumulator is
    // GONE. The trace stream is the single source of routed messages —
    // agent outboxes are trace-persisted at spawner.rs:489 and the
    // rework synthetic findings message is appended to the trace at the
    // rework dispatch site below. `TeamContext.messages` (per-iteration)
    // and `finalize_shipped_team`'s chain-trigger both reconstruct from
    // the trace via `messages_by_role_from_trace`. Pure-projection
    // invariant satisfied: a brief's routed-message state is fully
    // derivable from `(trace stream)`, no load-bearing in-process Vec.
    // #539 phase 6: permit-narrowing overrides are NOT accumulated in
    // process — `overrides_from_messages` resolves them per-role from
    // the trace-reconstructed inbound messages at spawn time.
    // Lazily allocated on first role that declares a `workspace_mount`.
    let mut workspace: Option<BriefWorkspace> = None;
    // Track the final team-level outcome.
    let mut team_shipped = true;
    // Captured at each `team_shipped = false` break so the terminal
    // `:state` write below can name the failing role (e.g.
    // `"reviewer-claude-agentry verdict=Failed"`). Operators reading the
    // `:state` key see WHY without grepping the trace stream.
    let mut team_failure_detail: Option<String> = None;
    'outer: loop {
        // Ready set: roles whose upstream roles are all shipped and
        // that have not yet shipped themselves. Roles with zero
        // inbound edges are immediately ready.
        //
        // v2 finale (#539 phase 7b): the FSM's `Walking.evidence` is
        // the SOLE authority for shipped-status and `retry.attempt`
        // for the rework budget — the in-process `shipped_roles` /
        // `reworks_used` accumulators and the union are deleted. The
        // `await_fsm_settled` barrier at the end of each iteration
        // guarantees the driver consumed the prior batch before this
        // read, so there is no lag window. Rework is the ratified v2
        // full-reset-to-entry model (FSM empties evidence + bumps
        // retry on the ReworkNeeded RoleDone, lifecycle.rs:516-519);
        // an empty shipped-set here ⇒ ready-set recomputes from the
        // entry node ⇒ the chain re-fires from the coder.
        // `read_walking_view` returns `None` when the FSM has reached
        // a terminal state (`Shipped` / `Failed`); the outer loop must
        // exit on `None` so a post-terminal iteration does not respawn
        // the entry role into a workspace whose commit has already
        // been merged (issue #562). The verdict-kind read after
        // `'outer` derives the final return.
        let Some((shipped_set, fsm_retry_attempt)) =
            read_walking_view(conn, &brief.id, &team).await
        else {
            break 'outer;
        };
        let ready: Vec<RoleRef> = team
            .roles
            .iter()
            .filter(|r| !shipped_set.contains(*r))
            .filter(|r| inbound_satisfied(r, &team, &shipped_set))
            .cloned()
            .collect();

        if ready.is_empty() {
            break;
        }

        // v2 finale (#539 phase 4): trace-derived inbox probe. Scan
        // the trace stream once per iteration and group routed
        // messages by destination role; below at TeamContext
        // construction, log divergence between this trace-derived
        // slice and the in-process `all_messages` slice. Behavior is
        // unchanged this commit — in-process slice remains the
        // authoritative source. Later phase swaps the authoritative
        // source and deletes `all_messages`.
        let trace_msgs_by_role = messages_by_role_from_trace(conn, &brief.id).await;

        // Setup phase (serial): fetch role records, allocate workspace if
        // needed, mint+narrow+sign permits, build per-role TeamContexts.
        let mut runs: Vec<RoleRun> = Vec::with_capacity(ready.len());
        for role_ref in &ready {
            let role = redis_io::fetch_role(conn, &role_ref.name, role_ref.version).await?;

            if role.workspace_mount.is_some() && workspace.is_none() {
                let repo = resolve_repo_for_brief(brief, conn, cfg).await?;
                let ws = workspace::allocate(
                    &brief.id,
                    repo.as_ref().map(|(u, b)| (u.as_str(), b.as_str())),
                )
                .await?;
                tracing::info!(
                    brief = %brief.id,
                    path = %ws.host_path.display(),
                    worktree = repo.is_some(),
                    "allocated brief workspace"
                );
                workspace = Some(ws);
            }

            let mut permit = mint_permit(brief, &role)?;
            // #539 phase 6: permit-narrowing override resolved from the
            // trace-reconstructed inbound messages (the per-iteration
            // `trace_msgs_by_role` slice for this role), NOT an
            // in-process `overrides_for` accumulator. Equivalence pinned
            // by tests/messages_by_role.rs.
            if let Some(o) = overrides_from_messages(
                &team,
                &role.name.0,
                trace_msgs_by_role
                    .get(&role.name.0)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            ) {
                apply_overrides(&mut permit, &o);
                tracing::info!(
                    brief = %brief.id,
                    role = %role_ref.name,
                    overrides = ?o,
                    "applied permit overrides from upstream (trace-derived)"
                );
            }
            permit_mod::sign(&mut permit, signing_key)?;

            // v2 finale (#539 phase 5b): the role's inbox is now read
            // from the trace stream (via the per-iteration
            // `trace_msgs_by_role` derivation), NOT the in-process
            // `all_messages` accumulator. The equivalence is pinned by
            // `tests/messages_by_role.rs` (#539 phase 5a) — the
            // trace-derived slice reproduces exactly what
            // `all_messages.iter().filter(|m| m.to == role).cloned()`
            // produced. `all_messages` is still WRITTEN below (rework
            // synthetic findings message + `finalize_shipped_team`
            // chain-trigger consume it); phase 5c migrates those two
            // off it and deletes the Vec.
            let team_ctx = TeamContext {
                messages: trace_msgs_by_role
                    .get(&role.name.0)
                    .cloned()
                    .unwrap_or_default(),
            };

            runs.push(RoleRun {
                role_ref: role_ref.clone(),
                role,
                permit,
                team_ctx,
            });
        }

        // Concurrent fan-out: each role gets its own ConnectionManager clone
        // so the spawner's `&mut ConnectionManager` borrows are disjoint.
        // ConnectionManager is Arc-internal; clones share the underlying
        // multiplexed connection.
        let mut role_conns: Vec<ConnectionManager> =
            (0..runs.len()).map(|_| conn.clone()).collect();

        let workspace_ref = workspace.as_ref();
        let futs: Vec<_> = runs
            .iter()
            .zip(role_conns.iter_mut())
            .map(|(run, conn_for_role)| {
                let ctx = RunAgentCtx {
                    brief,
                    role: &run.role,
                    permit: &run.permit,
                    verifying_key,
                    team_context: &run.team_ctx,
                    workspace: workspace_ref,
                    profile: resolved_profile.as_ref(),
                };
                spawner.run_agent(ctx, conn_for_role)
            })
            .collect();
        let outcomes = join_all(futs).await;

        // Outcome processing pass: append verdicts, accumulate outboxes,
        // propagate permit overrides, classify each role's verdict for the
        // state-update phase.
        let mut shipped_in_batch: Vec<RoleRef> = Vec::new();
        let mut reworks: Vec<(RoleRef, Vec<orchestrator_types::ReviewFinding>)> = Vec::new();
        let mut failures: Vec<RoleRef> = Vec::new();

        for (run, outcome_res) in runs.iter().zip(outcomes.into_iter()) {
            let outcome = outcome_res?;
            tracing::info!(
                brief = %brief.id,
                role = %run.role_ref.name,
                verdict = ?outcome.verdict.kind,
                outbox_len = outcome.outbox.len(),
                "role completed"
            );

            // #539 phase 5c + 6: `outcome.outbox` is NOT walked or
            // accumulated in process. Every message it carries was
            // already trace-persisted by `spawner::run_agent` at
            // spawner.rs:489 (`append_trace` on each
            // `EventKind::Message`). Inbox reconstruction
            // (`messages_by_role_from_trace`) and permit-override
            // resolution (`overrides_from_messages`, phase 6) both read
            // the trace; the edge/`permit_overrides_from` extraction
            // that used to live here now happens in
            // `overrides_from_messages` at spawn time. The field is
            // consumed only to move it out of `outcome`.
            let _ = outcome.outbox;

            // F6 escalation: when the coder reports verdict=Failed with
            // cause=self_review_disagreed, the translator has emitted
            // BriefEvent::CoderDisagreed and the FSM has flipped
            // Walking{Coder} → Walking{OperatorDecision}. The daemon
            // must NOT mark this as a team failure — that would
            // atomically overwrite the FSM's parked state with terminal
            // Failed, breaking captain decide. Dissolves #529 (the F6
            // routing regression caused by the daemon's mini-FSM
            // pre-empting the canonical lifecycle FSM). The spawner
            // propagates `cause:` prefixed reason onto the verdict, see
            // `spawner::compute_verdict`.
            if matches!(outcome.verdict.kind, VerdictKind::Failed)
                && outcome.verdict.reason.as_deref() == Some("cause:self_review_disagreed")
            {
                tracing::info!(
                    brief = %brief.id,
                    role = %run.role_ref.name,
                    "coder reported self_review_disagreed; team-orchestration yields to FSM (Walking{{OperatorDecision}}) for captain decide"
                );
                // Detach the workspace handle without tearing it down —
                // the FSM driver's cleanup fires on the eventual
                // terminal CaptainAccepted/CaptainRejected transition.
                let _ = workspace.take();
                return Ok(VerdictKind::Failed);
            }
            match outcome.verdict.kind {
                VerdictKind::Shipped => shipped_in_batch.push(run.role_ref.clone()),
                VerdictKind::ReworkNeeded { findings } => {
                    reworks.push((run.role_ref.clone(), findings));
                }
                _ => failures.push(run.role_ref.clone()),
            }
        }

        // #539 phase 7b: shipped-status is the FSM's `Walking.evidence`
        // alone (read at loop top via `read_walking_view`); the
        // in-process `shipped_roles` accumulator is deleted. The batch's
        // Shipped roles are still tracked locally in `shipped_in_batch`
        // — not as a source of truth, but as the `expect_shipped`
        // argument to the end-of-iteration `await_fsm_settled` barrier.

        // Reworks: rewind to the single upstream. v2 finale (#539
        // phase 7b): the FSM's `Walking.retry.attempt` is the sole
        // rework counter (the in-process `reworks_used` is deleted).
        // The barrier guarantees `fsm_retry_attempt` reflects the prior
        // batch, so it can be used directly without a max() against a
        // local mirror.
        let effective_reworks_used = fsm_retry_attempt.unwrap_or(0);
        // Barrier inputs (#539 phase 7b): the FSM attempt observed at
        // iteration top, whether this batch dispatched a rework, and
        // the roles that shipped this batch — passed to
        // `await_fsm_settled` at the end of the iteration.
        let started_attempt = fsm_retry_attempt.unwrap_or(0);
        let had_rework = !reworks.is_empty();
        let mut rewound_subdags: HashSet<RoleRef> = HashSet::new();
        for (from_ref, findings) in reworks {
            let upstream = resolve_rework_target(&from_ref, &team);
            match upstream {
                Some(up) if effective_reworks_used < team.max_retries => {
                    // #539 phase 5c: the rework findings message is
                    // daemon-synthesized (no agent emitted it). Append
                    // it to the trace stream with the trace `agent`
                    // field set to the source role's NAME (not a UUID)
                    // so `group_messages_by_role`'s daemon-attributed
                    // fallback reconstructs `from = from_ref.name.0` —
                    // exactly what the deleted in-process
                    // `all_messages.push(RoutedMessage { from:
                    // from_ref.name.0, .. })` produced. The trace is now
                    // the single source: the next outer-loop iteration's
                    // `messages_by_role_from_trace` delivers this into
                    // the rewound upstream's `TeamContext.messages`
                    // (post-#552 TeamContext reads only the trace, so
                    // without this the findings would never reach the
                    // rewound coder). `append_trace` is synchronous to
                    // Redis, so the entry is visible to the next
                    // iteration's XRANGE. A Redis error is logged WARN
                    // and the rework still proceeds via the FSM rewind —
                    // a degraded rework beats a crashed brief.
                    let rework_msg = orchestrator_types::Event::new(EventKind::Message {
                        to: up.name.0.clone(),
                        payload: serde_json::json!({ "findings": findings }),
                    });
                    if let Err(e) =
                        redis_io::append_trace(conn, &brief.id, &from_ref.name.0, &rework_msg).await
                    {
                        tracing::warn!(
                            brief = %brief.id,
                            from = %from_ref.name,
                            to = %up.name,
                            error = %e,
                            "rework findings message append_trace failed; rework proceeds without the findings payload"
                        );
                    }
                    let route_kind = if team
                        .incoming(&from_ref)
                        .iter()
                        .any(|e| e.rework_target.is_some())
                    {
                        "rework_target"
                    } else {
                        "fallback_upstream"
                    };
                    tracing::info!(
                        brief = %brief.id,
                        from = %from_ref.name,
                        to = %up.name,
                        findings_count = findings.len(),
                        effective_reworks_used,
                        fsm_attempt = ?fsm_retry_attempt,
                        max_retries = team.max_retries,
                        route_kind = %route_kind,
                        "rework requested — FSM full-reset to entry node"
                    );
                    // #539 phase 7b: the rewind itself is the FSM's job.
                    // On the `ReworkNeeded`/`Failed` RoleDone the FSM
                    // empties `Walking.evidence` and resets `node_id` to
                    // the entry node (lifecycle.rs:516-519, ratified v2
                    // full-reset model). The deleted
                    // `shipped_roles.retain(up)` + downstream-subdag
                    // retain implemented the OLD partial-rewind in
                    // process; under FSM authority the next settled
                    // iteration's empty evidence re-fires the whole
                    // chain from the coder. `rewound_subdags` is still
                    // populated so the failures-squash guard below knows
                    // these roles are mid-rewind (a Failed verdict for a
                    // role in the rewound set is not fatal — it re-runs).
                    rewound_subdags.insert(up.clone());
                    for r in downstream_subdag(&up, &team) {
                        rewound_subdags.insert(r);
                    }
                }
                Some(up) => {
                    tracing::warn!(
                        brief = %brief.id,
                        role = %from_ref.name,
                        upstream = %up.name,
                        effective_reworks_used,
                        fsm_attempt = ?fsm_retry_attempt,
                        max_retries = team.max_retries,
                        "rework requested but retry budget exhausted"
                    );
                    team_shipped = false;
                    team_failure_detail = Some(format!(
                        "{} rework budget exhausted (attempt={}, max={})",
                        from_ref.name, effective_reworks_used, team.max_retries
                    ));
                    break 'outer;
                }
                None => {
                    tracing::warn!(
                        brief = %brief.id,
                        role = %from_ref.name,
                        "rework requested but role has no upstream — treating as failed"
                    );
                    team_shipped = false;
                    team_failure_detail = Some(format!(
                        "{} requested rework with no upstream",
                        from_ref.name
                    ));
                    break 'outer;
                }
            }
        }

        // Failures: only fatal if not already part of an active rewind
        // sub-DAG (in which case the failure is squashed and the role
        // re-fires once its upstream re-ships).
        for failed in &failures {
            if !rewound_subdags.contains(failed) {
                team_shipped = false;
                team_failure_detail = Some(format!("{} verdict=Failed", failed.name));
                break 'outer;
            }
        }

        // #539 phase 7b sync barrier: wait until the FSM driver has
        // consumed this batch before the next iteration's
        // `read_walking_view` reads `Walking.evidence`. Without this
        // the loop would read stale FSM state (the deleted in-process
        // accumulators previously covered that lag window). Bounded;
        // see `await_fsm_settled` / `fsm_settled`.
        //
        // On timeout the brief MUST fail: proceeding with stale
        // `Walking.evidence` causes the loop to respawn already-shipped
        // roles, whose second-run coder correctly emits `no_changes`
        // and fails the team — discarding the first run's valid
        // commit. That was the root cause of every trivial-doc brief
        // failure on `agentry-self-host-v0` between #559 landing and
        // this fix.
        let settled = await_fsm_settled(
            conn,
            &brief.id,
            started_attempt,
            &shipped_in_batch,
            had_rework,
        )
        .await;
        if !settled {
            team_shipped = false;
            team_failure_detail = Some(format!(
                "fsm_settled barrier timeout after ~10min (started_attempt={started_attempt} \
                 had_rework={had_rework} shipped_in_batch={shipped_in_batch_count})",
                shipped_in_batch_count = shipped_in_batch.len()
            ));
            break 'outer;
        }
    }

    // Success requires the terminal role to have shipped — read from
    // the FSM (the in-process `shipped_roles` is deleted, #539 7b).
    // The walker only reaches `BriefState::Shipped` when the terminal
    // node ships; a non-terminal `Walking` with the terminal role
    // already `Shipped` in evidence is the equivalent pre-collapse
    // success signal. Anything else (Failed, Submitted, Walking
    // without the terminal role shipped) is a non-ship.
    if team_shipped {
        let terminal_shipped = match read_brief_state(conn, &brief.id).await {
            Ok(Some(rec)) => match rec.state {
                BriefState::Shipped => true,
                BriefState::Walking { evidence, .. } => evidence
                    .get(&NodeId(team.terminal_role.name.0.clone()))
                    .map(|v| matches!(v, EventVerdict::Shipped))
                    .unwrap_or(false),
                _ => false,
            },
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(
                    brief = %brief.id,
                    error = %e,
                    "terminal-role FSM read failed; treating as non-ship"
                );
                false
            }
        };
        if !terminal_shipped {
            team_shipped = false;
            team_failure_detail = Some(format!(
                "terminal role {} did not ship",
                team.terminal_role.name
            ));
        }
    }

    // Bail early on team failure — no chain-trigger. The workspace is
    // intentionally NOT torn down here: the lifecycle FSM's
    // `lifecycle_driver::cleanup_failed_brief` is the canonical
    // terminal-Failed cleanup site (see L.4 / EPIC #246). It removes
    // both the worktree dir and the `auto/<brief_id>` branch once the
    // FSM observes the terminal Failed transition. The log message here
    // names that handoff so an operator grepping the log still finds a
    // pointer to where cleanup actually runs.
    if !team_shipped {
        // Atomically write `BriefState::Failed { AcceptanceFailed }` to
        // `:state` BEFORE returning. Without this, the FSM driver's
        // projector path may never observe a terminating event (the
        // outer loop returns first and the late-reviewer-in-Reworking
        // branch parks the FSM), leaving the reaper firing
        // BudgetExhausted forever on a brief whose container is long
        // gone. See brief: zombie :state — terminal disposition must
        // atomically write Failed.
        let detail = team_failure_detail.clone().unwrap_or_else(|| {
            "team-orchestration declared failure without per-role detail".to_string()
        });
        let mut projector = RedisStateProjector::new(conn.clone(), brief.id.clone());
        write_team_terminal_state(
            &mut projector,
            &brief.id,
            BriefState::Failed {
                reason: Reason::AcceptanceFailed {
                    detail: detail.clone(),
                },
            },
            "team-orchestration-failed",
        )
        .await;
        if let Some(ws) = workspace.take() {
            tracing::info!(
                brief = %brief.id,
                path = %ws.host_path.display(),
                detail = %detail,
                "workspace handoff: lifecycle FSM driver cleans up on terminal Failed"
            );
        }
        return Ok(VerdictKind::Failed);
    }

    // Atomically write `BriefState::Shipped` to `:state` BEFORE
    // returning. Without this, the FSM driver may still be parked on
    // `source.next().await` when the team-orchestration outer loop
    // declares team_shipped=true and tears down the workspace, leaving
    // the reaper firing BudgetExhausted on a brief that has shipped.
    {
        let mut projector = RedisStateProjector::new(conn.clone(), brief.id.clone());
        write_team_terminal_state(
            &mut projector,
            &brief.id,
            BriefState::Shipped,
            "team-orchestration-shipped",
        )
        .await;
    }

    // Chain-trigger BEFORE workspace destruction: chain paths often live
    // INSIDE the workspace (e.g. planner emits next_brief_refs into
    // <workspace>/planner-children/), so file reads must complete while the
    // workspace still exists. Destruction follows once every brief is parsed
    // into memory and submitted to Redis.
    finalize_shipped_team(conn, brief, workspace.take()).await?;

    Ok(VerdictKind::Shipped)
}

/// Atomically write a terminal `BriefStateRecord` through the supplied
/// projector. The projector is responsible for the underlying
/// three-key Lua atomic write (state_log XADD, state SET, cursor SET).
/// Errors are logged at WARN — the team-orchestration outer loop's
/// terminal-disposition path must not propagate Redis hiccups, since
/// the brief's verdict is already decided and surfacing an error here
/// would mask a Shipped/Failed outcome with a generic handler error.
/// Public so `tests/lifecycle.rs` can drive the helper with the
/// in-memory projector and pin the terminal-record shape.
pub async fn write_team_terminal_state(
    projector: &mut (dyn StateProjector + Send),
    brief_id: &BriefId,
    state: BriefState,
    cursor: &str,
) {
    let record = BriefStateRecord {
        brief_id: brief_id.clone(),
        state,
        parent_brief_id: None,
        composition_role: None,
        at: now(),
    };
    if let Err(e) = projector.write(&record, cursor).await {
        tracing::warn!(
            brief = %brief_id.0,
            error = %e,
            cursor = %cursor,
            "team-orchestration: terminal :state atomic write failed",
        );
    }
}

/// Post-shipping finalize: read every chain-trigger brief into memory and
/// submit it to Redis BEFORE destroying the workspace. The ordering is
/// load-bearing — chain paths can point inside the workspace, so destroying
/// first would ENOENT every read. See the
/// `chain_trigger_runs_before_workspace_destruction` regression test.
async fn finalize_shipped_team(
    conn: &mut ConnectionManager,
    brief: &Brief,
    workspace: Option<BriefWorkspace>,
) -> Result<()> {
    // #539 phase 5c: chain-trigger paths are derived from the trace,
    // not an in-process accumulator. `collect_chain_paths` scans the
    // brief payload plus every routed-message payload for
    // `next_brief_refs`; the routed messages are reconstructed from the
    // trace (the canonical source — spawner.rs:489 trace-persists every
    // agent `Message`, and the rework synthetic message is appended to
    // the trace at the rework dispatch site above). The grouped map is
    // flattened back to a flat `Vec<RoutedMessage>` since chain-path
    // extraction does not care which role a message was addressed to.
    let trace_messages: Vec<RoutedMessage> = messages_by_role_from_trace(conn, &brief.id)
        .await
        .into_values()
        .flatten()
        .collect();
    for next_ref in collect_chain_paths(&brief.payload, &trace_messages) {
        if let Some(next_brief) = load_next_brief(&next_ref).await {
            redis_io::submit_brief(conn, &next_brief).await?;
            tracing::info!(
                from = %brief.id,
                next = %next_brief.id,
                path = %next_ref,
                "chain trigger: next brief submitted"
            );
        }
    }

    if let Some(ws) = workspace {
        // The team shipped — this finalize path only runs when team_shipped is
        // true, so the operator-visible verdict for this brief is "shipped".
        // Route through `disposition_for` to keep the daemon's teardown rule
        // aligned with the disposition table even as future verdict variants
        // (e.g. "review-blocked-*") are added.
        let disposition = workspace::disposition_for("shipped");
        if let Err(e) = workspace::destroy_with_disposition(&ws, disposition).await {
            tracing::warn!(
                brief = %brief.id,
                path = %ws.host_path.display(),
                error = %e,
                "workspace destroy failed"
            );
        } else {
            tracing::info!(
                brief = %brief.id,
                path = %ws.host_path.display(),
                ?disposition,
                "workspace teardown routed (team shipped)"
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon-Orchestrated Lifecycle (DOL) — see closes #50.
//
// Three Redis keys per meta-brief:
// * `agentry:brief:<meta_id>:children_pending`  (set of child brief ids)
// * `agentry:brief:<meta_id>:children_verdicts` (list of JSON Verdicts)
// * `agentry:brief:<meta_id>:verifier_pending`  (single brief id, optional)
// * `agentry:brief:<meta_id>:verifier_verdict`  (single JSON Verdict, optional)
//
// `submit_brief` registers parent_brief children in the pending set BEFORE the
// XADD so a child can never reach terminal verdict ahead of its registration.
// `dol_on_brief_terminal` runs after `handle_brief` returns and:
// * if the brief has `payload.verifies_brief_id` → it IS a verifier; record
//   its verdict against the meta-brief and call `compose_meta_verdict`;
// * else if it has `parent_brief = meta_id` → record its verdict in the meta's
//   children_verdicts list, decrement pending; if pending hit 0, call
//   `on_all_children_resolved`.
// `on_all_children_resolved` synthesizes a verifier brief if the meta-brief
// declared `success_criteria`, otherwise composes immediately.
// `compose_meta_verdict` reads the accumulated state, picks a final
// kind+reason via the pure `compose_verdict_parts`, emits one Verdict for the
// meta-brief on `agentry:verdicts`, and deletes the four helper keys.
// ---------------------------------------------------------------------------

/// Write `agentry:brief:<id>:body <json>` for the given brief.
///
/// The dashboard's `active_briefs()` does SMEMBERS+MGET against this
/// key, so its render path depends on the body key being present.
/// `redis_io::submit_brief` writes it on the API path; this helper is
/// the daemon-side defensive backfill called from the stream-intake
/// loop so direct-XADD callers (operator tooling, captain scripts,
/// replay/recovery) don't leave the dashboard reading 'No briefs in
/// flight' while the daemon happily processes the brief.
///
/// Idempotent: SET overwrites whatever `submit_brief` wrote with the
/// identical body. The SET error is swallowed and logged at WARN —
/// the body key is render-side, not correctness-side, so a transient
/// Redis hiccup must not abort intake. A serialization failure does
/// propagate (it would indicate a Brief that can't round-trip JSON).
///
/// `pub` so the integration test in
/// `tests/daemon_intake_body_backfill.rs` can drive it directly with
/// a real Redis connection (matches the `compose_meta_verdict`
/// pattern in `tests/daemon_test.rs`).
pub async fn backfill_body_key(conn: &mut ConnectionManager, brief: &Brief) -> Result<()> {
    let body_json = serde_json::to_string(brief)?;
    let body_key = format!("agentry:brief:{}:body", brief.id.0);
    if let Err(e) = conn.set::<_, _, ()>(&body_key, &body_json).await {
        tracing::warn!(brief = %brief.id.0, error = %e, "failed to backfill body key");
    }
    Ok(())
}

/// Emit the brief-1b intake rejection signal: a `Failed` verdict on
/// `agentry:verdicts` plus a `BriefRejected` trace event on the brief's
/// trace stream. Best-effort — Redis hiccups are logged, never propagated
/// (the alternative is leaving the brief running through downstream
/// gates after we've already chosen to reject).
async fn emit_brief_rejected(
    conn: &mut ConnectionManager,
    brief: &Brief,
    err: &IntakeError,
    rejected_target_repo: Option<&TargetRepo>,
) {
    let reason_code = match err {
        IntakeError::MissingTargetRepo => "MissingTargetRepo",
        IntakeError::OwnerNotAllowed { .. } => "OwnerNotAllowed",
    };
    let detail = format!("intake-reject: {err}");
    let verdict = Verdict::new(brief.id.clone(), VerdictKind::Failed).with_reason(detail.clone());
    if let Err(e) = redis_io::append_verdict(conn, &verdict).await {
        tracing::error!(
            brief = %brief.id,
            error = %e,
            "intake-reject: failed to append Failed verdict to verdicts stream",
        );
    }
    let target_repo_label = rejected_target_repo
        .map(TargetRepo::display_qualified)
        .unwrap_or_default();
    let payload = serde_json::json!({
        "kind": "BriefRejected",
        "reason": reason_code,
        "detail": detail,
        "target_repo": target_repo_label,
        "cohort_labels": brief.cohort_labels,
    });
    let event = orchestrator_types::Event::new(EventKind::Event { payload });
    if let Err(e) = redis_io::append_trace(conn, &brief.id, "daemon", &event).await {
        tracing::warn!(
            brief = %brief.id,
            error = %e,
            "intake-reject: failed to append BriefRejected trace event",
        );
    }
    tracing::warn!(
        brief = %brief.id,
        reason = reason_code,
        target_repo = %target_repo_label,
        "intake-reject: brief rejected at pre-mint gate",
    );
}

const DOL_VERIFIER_TOPOLOGY: &str = "agentry-verify-v0";

/// Hook called once per brief when it reaches terminal verdict (success or
/// failure). Wires the brief into the meta-brief's lifecycle if it carries
/// either `payload.verifies_brief_id` (this brief IS a verifier) or
/// `parent_brief = Some(meta_id)` (this brief is a child of a meta-brief).
async fn dol_on_brief_terminal(
    conn: &mut ConnectionManager,
    brief: &Brief,
    kind: &VerdictKind,
) -> Result<()> {
    let verdict = Verdict::new(brief.id.clone(), kind.clone());
    let verdict_json = serde_json::to_string(&verdict)?;

    if let Some(meta_id) = brief
        .payload
        .get("verifies_brief_id")
        .and_then(|v| v.as_str())
    {
        let verdict_key = format!("agentry:brief:{meta_id}:verifier_verdict");
        let pending_key = format!("agentry:brief:{meta_id}:verifier_pending");
        let _: () = conn.set(&verdict_key, verdict_json.as_str()).await?;
        let _: () = conn.del(&pending_key).await?;
        tracing::info!(
            verifier = %brief.id,
            meta = %meta_id,
            kind = ?kind,
            "DOL: verifier verdict recorded; composing meta verdict"
        );
        compose_meta_verdict(conn, meta_id).await?;
        return Ok(());
    }

    if let Some(meta_id) = &brief.parent_brief {
        let pending_key = format!("agentry:brief:{}:children_pending", meta_id.0);
        let verdicts_key = format!("agentry:brief:{}:children_verdicts", meta_id.0);
        let _: () = conn.srem(&pending_key, brief.id.0.as_str()).await?;
        let _: () = conn.rpush(&verdicts_key, verdict_json.as_str()).await?;
        let pending: i64 = conn.scard(&pending_key).await?;
        tracing::info!(
            child = %brief.id,
            meta = %meta_id,
            kind = ?kind,
            pending_remaining = pending,
            "DOL: child verdict recorded"
        );
        if pending == 0 {
            on_all_children_resolved(conn, &meta_id.0).await?;
        }
    }

    Ok(())
}

/// Called when the last child of a meta-brief resolves. If the meta-brief
/// declared `success_criteria`, synthesize and dispatch a verifier brief
/// (which will compose the meta verdict once IT resolves). Otherwise compose
/// immediately from the children's verdicts alone.
async fn on_all_children_resolved(conn: &mut ConnectionManager, meta_id: &str) -> Result<()> {
    let meta_brief = redis_io::fetch_brief_body(conn, meta_id).await?;

    let criterion = meta_brief
        .payload
        .get("success_criteria")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(criterion) = criterion {
        // If any child failed, skip the verifier — there's no point running
        // criterion against a broken state. Compose directly.
        if !children_all_shipped(conn, meta_id).await? {
            tracing::info!(
                meta = %meta_id,
                "DOL: at least one child failed — skipping verifier dispatch"
            );
            return compose_meta_verdict(conn, meta_id).await;
        }

        let target_repo = meta_brief.target_repo().map(|t| t.to_string());
        let base_branch = meta_brief
            .payload
            .get("base_branch")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let verifier_id = format!("brf_verify_{}_{}", meta_id, now().timestamp());
        let mut payload_obj = serde_json::Map::new();
        payload_obj.insert(
            "success_criteria".into(),
            serde_json::Value::String(criterion),
        );
        payload_obj.insert(
            "verifies_brief_id".into(),
            serde_json::Value::String(meta_id.into()),
        );
        if let Some(t) = target_repo {
            payload_obj.insert("target_repo".into(), serde_json::Value::String(t));
        }
        if let Some(b) = base_branch {
            payload_obj.insert("base_branch".into(), serde_json::Value::String(b));
        }
        payload_obj.insert(
            "issue_title".into(),
            serde_json::Value::String(format!("verify {meta_id}")),
        );
        payload_obj.insert(
            "issue_body".into(),
            serde_json::Value::String("auto-synthesized verifier".into()),
        );

        let verifier_brief = Brief {
            id: BriefId(verifier_id.clone()),
            project: meta_brief.project.clone(),
            topology: VersionedRef::new(DOL_VERIFIER_TOPOLOGY, 1),
            payload: serde_json::Value::Object(payload_obj),
            kind: None,
            contract: None,
            budget: Budget {
                max_tokens: None,
                max_wall_seconds: Some(600),
                max_usd: None,
            },
            escalation: meta_brief.escalation,
            // Verifier is in its own DOL slot — NOT a child of the meta-brief.
            parent_brief: None,
            cohort_labels: meta_brief.cohort_labels.clone(),
            redeploy_required: vec![],
            submitted_by: "daemon-dol-verifier".into(),
            submitted_at: now(),
        };

        let pending_key = format!("agentry:brief:{meta_id}:verifier_pending");
        let _: () = conn.set(&pending_key, verifier_id.as_str()).await?;
        redis_io::submit_brief(conn, &verifier_brief).await?;
        tracing::info!(
            meta = %meta_id,
            verifier = %verifier_id,
            "DOL: verifier brief synthesized and dispatched"
        );
        Ok(())
    } else {
        tracing::info!(
            meta = %meta_id,
            "DOL: no success_criteria — composing meta verdict from children only"
        );
        compose_meta_verdict(conn, meta_id).await
    }
}

/// Read children + verifier state from Redis, compose the meta-brief's final
/// verdict, append it to `agentry:verdicts`, and clean up the helper keys.
///
/// Idempotency: atomically claims `agentry:meta_verdict:emitted:<meta_id>`
/// via `SET ... NX EX 86400` immediately before the verdict XADD. If the
/// marker is already set, returns early without emitting. This guards the
/// concurrent path where multiple terminal-handlers (e.g. three children
/// resolving in the same tick) all reach this function for the same meta;
/// it also guards stale re-entries that arrive AFTER the helper-key
/// cleanup below, since the marker is intentionally not cleaned up — the
/// 24h TTL is its retention.
///
/// `pub` so the integration test in `tests/daemon_test.rs` can drive the
/// gate directly with a real Redis connection.
#[tracing::instrument(skip(conn), fields(meta = %meta_id))]
pub async fn compose_meta_verdict(conn: &mut ConnectionManager, meta_id: &str) -> Result<()> {
    // Entry log makes the compose-call observable from production traces so
    // duplicate-compose incidents can be correlated with the caller's span
    // chain (#178). The `instrument` attribute carries `meta_id` through
    // every event emitted by this function.
    tracing::info!("DOL: compose_meta_verdict entered");

    let verdicts_key = format!("agentry:brief:{meta_id}:children_verdicts");
    let pending_key = format!("agentry:brief:{meta_id}:children_pending");
    let verifier_pending_key = format!("agentry:brief:{meta_id}:verifier_pending");
    let verifier_verdict_key = format!("agentry:brief:{meta_id}:verifier_verdict");
    let xadd_emitted_key = format!("agentry:meta_verdict:emitted:{meta_id}");

    let raw_children: Vec<String> = conn.lrange(&verdicts_key, 0, -1).await?;
    let children: Vec<Verdict> = raw_children
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();

    let raw_verifier: Option<String> = conn.get(&verifier_verdict_key).await?;
    let verifier: Option<Verdict> = raw_verifier
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());

    let (final_kind, reason) = compose_verdict_parts(&children, verifier.as_ref());
    let mut final_verdict = Verdict::new(BriefId(meta_id.into()), final_kind.clone());
    final_verdict.reason = reason;

    // #311 fence (last-line-of-defense): if every finding on a
    // ReworkNeeded verdict has empty message+requirements+prohibitions,
    // an upstream reviewer produced parse-failure noise rather than a
    // real defect — downgrade to Shipped so the rework loop doesn't
    // churn. Belt + suspenders alongside the reviewer-side fence in
    // `agentry-role-runtime`.
    let n_dropped = downgrade_empty_rework(&mut final_verdict.kind);
    if n_dropped > 0 {
        tracing::warn!(
            brief = %final_verdict.brief.0,
            n_dropped,
            "compose_meta_verdict downgraded ReworkNeeded->Shipped (all findings empty)"
        );
    }

    // Atomic claim immediately before the XADD: only the first arrival
    // for a given meta_id wins SET NX and emits the verdict. The marker
    // is NOT deleted in the cleanup block below — its 24h TTL is the
    // retention. Concurrent terminal callbacks have been observed
    // re-entering this function (A7v3 reproducer); 'composer is called
    // once' is not a safe argument.
    let claimed: bool = redis::cmd("SET")
        .arg(&xadd_emitted_key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(86400)
        .query_async(conn)
        .await?;
    if !claimed {
        tracing::info!(
            meta = %meta_id,
            "DOL: meta verdict XADD already emitted (idempotency gate); skipping"
        );
        return Ok(());
    }
    let stream_id = redis_io::append_verdict(conn, &final_verdict).await?;
    tracing::info!(
        brief = %final_verdict.brief.0,
        kind = ?final_verdict.kind,
        stream_id = %stream_id,
        "meta verdict emitted"
    );

    let _: () = conn.del(&verdicts_key).await?;
    let _: () = conn.del(&pending_key).await?;
    let _: () = conn.del(&verifier_pending_key).await?;
    let _: () = conn.del(&verifier_verdict_key).await?;

    tracing::info!(
        meta = %meta_id,
        kind = ?final_kind,
        children = children.len(),
        verifier = verifier.is_some(),
        "DOL: meta verdict composed"
    );
    Ok(())
}

/// Pure compose: given the children's verdicts and an optional verifier
/// verdict, return the meta-brief's final `(kind, reason)`. Extracted so the
/// composition rules can be unit-tested without a Redis instance.
fn compose_verdict_parts(
    children: &[Verdict],
    verifier: Option<&Verdict>,
) -> (VerdictKind, Option<String>) {
    let children_all_shipped = children
        .iter()
        .all(|v| matches!(v.kind, VerdictKind::Shipped));
    if !children_all_shipped {
        return (
            VerdictKind::Failed,
            Some("one or more children failed".into()),
        );
    }
    if let Some(v) = verifier {
        let suffix = v.reason.as_deref().unwrap_or("(no reason)");
        return (v.kind.clone(), Some(format!("verifier: {suffix}")));
    }
    (VerdictKind::Shipped, None)
}

/// Daemon-side last-line-of-defense for #311: if `kind` is
/// `ReworkNeeded` whose findings are non-empty but every entry has
/// all-empty content (`message`, `requirements`, AND `prohibitions`),
/// downgrade it to `Shipped` and return the count of dropped findings.
/// Otherwise leave `kind` untouched and return 0.
///
/// Empty-Blocker findings are a parse failure upstream (reviewer-claude
/// emitted nominally-structured output that decodes to a hollow
/// finding); publishing them as `ReworkNeeded` produces a respawned
/// coder with no actionable signal and drains the retry budget on
/// noise. Belt + suspenders alongside the reviewer-side fence in
/// `agentry_role_runtime::drop_empty_blocker_findings`.
///
/// Crate-private; the existing live-Redis test in `tests/daemon_test.rs`
/// exercises it through `compose_meta_verdict`.
fn downgrade_empty_rework(kind: &mut VerdictKind) -> usize {
    let findings = match kind {
        VerdictKind::ReworkNeeded { findings } => findings,
        _ => return 0,
    };
    if findings.is_empty() {
        return 0;
    }
    let all_empty = findings
        .iter()
        .all(|f| f.message.is_empty() && f.requirements.is_empty() && f.prohibitions.is_empty());
    if !all_empty {
        return 0;
    }
    let n = findings.len();
    *kind = VerdictKind::Shipped;
    n
}

/// Read the meta-brief's children_verdicts list and return true iff every
/// recorded child verdict was Shipped. Used to short-circuit verifier
/// dispatch when at least one child already failed.
async fn children_all_shipped(conn: &mut ConnectionManager, meta_id: &str) -> Result<bool> {
    let verdicts_key = format!("agentry:brief:{meta_id}:children_verdicts");
    let raw: Vec<String> = conn.lrange(&verdicts_key, 0, -1).await?;
    Ok(raw
        .iter()
        .filter_map(|s| serde_json::from_str::<Verdict>(s).ok())
        .all(|v| matches!(v.kind, VerdictKind::Shipped)))
}

/// Combine chain-trigger paths from the brief payload and every accumulated
/// role-outbox message, then de-duplicate while preserving first-seen order.
fn collect_chain_paths(
    brief_payload: &serde_json::Value,
    messages: &[RoutedMessage],
) -> Vec<String> {
    let mut paths = next_brief_paths(brief_payload);
    for msg in messages {
        paths.extend(next_brief_paths(&msg.payload));
    }
    let mut seen: HashSet<String> = HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    paths
}

/// Extract the ordered list of next-brief file paths from `payload`.
/// Plural `next_brief_refs` (array of strings) takes precedence; falls back
/// to singular `next_brief_ref` for backward compatibility. Non-string
/// entries in the plural array are logged at WARN and skipped.
fn next_brief_paths(payload: &serde_json::Value) -> Vec<String> {
    if let Some(arr) = payload.get("next_brief_refs").and_then(|v| v.as_array()) {
        let mut paths = Vec::with_capacity(arr.len());
        for v in arr {
            match v.as_str() {
                Some(s) => paths.push(s.to_string()),
                None => {
                    tracing::warn!(value = %v, "chain: next_brief_refs entry is not a string; skipping");
                }
            }
        }
        return paths;
    }
    if let Some(s) = payload.get("next_brief_ref").and_then(|v| v.as_str()) {
        return vec![s.to_string()];
    }
    Vec::new()
}

/// Read and deserialize a Brief from `path`. Returns `None` on read or parse
/// failure; each is logged at WARN so the chain-trigger loop can skip the
/// bad entry without aborting the others.
async fn load_next_brief(path: &str) -> Option<Brief> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => match serde_json::from_str::<Brief>(&text) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(path=%path, error=%e, "chain: next brief JSON parse failed");
                None
            }
        },
        Err(e) => {
            tracing::warn!(path=%path, error=%e, "chain: next brief file read failed");
            None
        }
    }
}

/// True iff every upstream role of `role` is in `shipped`. Roles with no
/// inbound edges are trivially satisfied.
fn inbound_satisfied(role: &RoleRef, team: &TeamTopology, shipped: &HashSet<RoleRef>) -> bool {
    team.inbound_roles(role)
        .iter()
        .all(|up| shipped.contains(*up))
}

/// All roles reachable from `role` via outbound edges in `team.message_graph`.
/// Used by rework: when `role` is rewound to Pending, every role in its
/// downstream sub-DAG must also be reset to Pending so the slice re-fires
/// once `role` re-ships.
fn downstream_subdag(role: &RoleRef, team: &TeamTopology) -> HashSet<RoleRef> {
    let mut out: HashSet<RoleRef> = HashSet::new();
    let mut stack: Vec<RoleRef> = team.outgoing(role).iter().map(|e| e.to.clone()).collect();
    while let Some(r) = stack.pop() {
        if out.insert(r.clone()) {
            for e in team.outgoing(&r) {
                stack.push(e.to.clone());
            }
        }
    }
    out
}

/// Pick the role to rewind to when `from_ref` emitted ReworkNeeded.
///
/// Priority order:
///   1. If ANY incoming edge to `from_ref` has `rework_target: Some(target)`,
///      return that target. (If multiple incoming edges set different targets,
///      pick the FIRST one in `team.message_graph[]` order — deterministic
///      and operator-debuggable. Workflow authors should not normally set
///      conflicting targets; the validator does not enforce uniqueness in
///      this brief, see follow-up note.)
///   2. Otherwise fall back to the immediate upstream (`incoming.first().from`).
///   3. Returns None if `from_ref` has no incoming edges at all.
fn resolve_rework_target(from_ref: &RoleRef, team: &TeamTopology) -> Option<RoleRef> {
    let incoming = team.incoming(from_ref);
    for edge in &incoming {
        if let Some(target) = &edge.rework_target {
            return Some(target.clone());
        }
    }
    incoming.first().map(|e| e.from.clone())
}

/// Mint an unsigned permit from the brief's budget and the role's declared
/// scope. The caller signs it via `permit::sign` before handing it to the
/// spawner.
fn mint_permit(brief: &Brief, role: &AgentRole) -> Result<WorkPermit> {
    let agent_id = format!("agt_{}", uuid::Uuid::now_v7());
    let permit_id = format!("prm_{}", uuid::Uuid::now_v7());
    let expires_at = now() + chrono::Duration::hours(2);
    Ok(WorkPermit {
        permit_id,
        agent_id,
        role: role.name.clone(),
        brief: brief.id.clone(),
        tool_allowlist: role.tool_allowlist.clone(),
        allowed_tools: role.allowed_tools.clone(),
        permit_scope: role.permit_scope.clone(),
        max_tokens: brief.budget.max_tokens,
        max_wall_seconds: brief.budget.max_wall_seconds,
        max_usd: brief.budget.max_usd,
        expires_at,
        issued_at: now(),
        signature: None,
    })
}

/// Resolve the `(repo_url, base_branch)` tuple for this brief's workspace.
///
/// Priority:
/// 1. `brief.project` (when present) → fetch the Project record; use its
///    `repo_url` + `base_branch` if both are set.
/// 2. `brief.payload.target_repo` + `brief.payload.base_branch` (legacy
///    path) → construct a token-bearing forge URL using `GITEA_TOKEN`. The
///    token is needed only for the FIRST `git clone --bare`; subsequent
///    fetches+worktree-adds against the bare clone don't carry auth.
///
/// Returns `Ok(None)` if neither path yields a usable pair — in which case
/// the workspace falls back to an empty scratch dir (probe semantics).
async fn resolve_repo_for_brief(
    brief: &Brief,
    conn: &mut ConnectionManager,
    cfg: &crate::Config,
) -> Result<Option<(String, String)>> {
    if let Some(slug) = brief.project.as_deref() {
        match redis_io::fetch_project(conn, slug).await {
            Ok(project) => {
                if let (Some(url), Some(branch)) = (project.repo_url, project.base_branch) {
                    return Ok(Some((url, branch)));
                }
            }
            Err(Error::NotFound { .. }) => {
                // Project not found in Redis; fall through to payload path.
            }
            Err(e) => return Err(e),
        }
    }

    let target_repo = brief.target_repo();
    let base_branch = brief
        .payload
        .get("base_branch")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let forge_host = match brief.payload.get("forge_host").and_then(|v| v.as_str()) {
        Some(h) => h.to_string(),
        None => match cfg.forge.default_host.as_deref() {
            Some(h) => h.to_string(),
            None => {
                return Err(Error::Config(
                    "forge_host required: set brief.payload.forge_host or [forge] default_host in agentry.toml"
                        .into(),
                ));
            }
        },
    };

    if let (Some(repo), Some(branch)) = (target_repo, base_branch) {
        let url = forge_url(&repo, &forge_host)?;
        return Ok(Some((url, branch)));
    }

    Ok(None)
}

/// Slice I/2b: resolve `.agentry/profile.toml` for the brief's target_repo.
///
/// Skips the network call when any of `target_repo`, `cfg.forge.default_host`,
/// or `GITEA_TOKEN` is absent — the profile is optional and the brief should
/// proceed using role defaults in those cases. Logs the outcome at INFO
/// (success/absent/skipped) or WARN (fetch error). The returned `Option<Profile>`
/// is captured into a local in `handle_brief` and is not yet consumed; slice
/// I/2c will thread it to the spawner.
async fn fetch_brief_profile(
    brief: &Brief,
    cfg: &crate::Config,
) -> Option<orchestrator_types::Profile> {
    let target_repo = match brief.target_repo() {
        Some(t) => t,
        None => {
            tracing::warn!(
                brief = %brief.id,
                "profile fetch skipped: brief target_repo absent or rejected by strict validation",
            );
            return None;
        }
    };
    let target_repo_str = target_repo.to_string();
    let base_branch = brief
        .payload
        .get("base_branch")
        .and_then(|v| v.as_str())
        .unwrap_or("develop");
    let forge_host = cfg.forge.default_host.as_deref().unwrap_or("");
    let gitea_token = std::env::var("GITEA_TOKEN").unwrap_or_default();

    if forge_host.is_empty() || gitea_token.is_empty() {
        tracing::info!(
            brief = %brief.id,
            target_repo = %target_repo_str,
            forge_host = %forge_host,
            has_token = !gitea_token.is_empty(),
            "profile fetch skipped: missing forge_host/GITEA_TOKEN"
        );
        return None;
    }

    match redis_io::fetch_profile(
        &target_repo_str,
        base_branch,
        forge_host,
        &gitea_token,
        cfg.forge.tls_insecure,
    )
    .await
    {
        Ok(Some(p)) => {
            tracing::info!(
                brief = %brief.id,
                target_repo = %target_repo_str,
                tool_packs_coder = ?p.coder.tool_packs,
                tool_packs_reviewer = ?p.reviewer.tool_packs,
                acceptance_default = ?p.acceptance.default,
                gates = ?p.methodology.gates,
                "fetched .agentry/profile.toml from target_repo"
            );
            Some(p)
        }
        Ok(None) => {
            tracing::info!(
                brief = %brief.id,
                target_repo = %target_repo_str,
                "no .agentry/profile.toml in target_repo; using defaults"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                brief = %brief.id,
                target_repo = %target_repo_str,
                error = %e,
                "profile fetch failed; using defaults"
            );
            None
        }
    }
}

/// Build a token-bearing forge URL for the FIRST bare clone. Subsequent
/// worktree operations against the bare clone do not need to carry auth.
///
/// Brief 1b: the path component is composed via [`TargetRepo::clone_url`]
/// from the validated `(owner, repo)` fields rather than from the raw
/// payload string — this closes the URL-fragment injection vector G3.
/// The token-bearing `oauth2:<token>@host` form is preserved by
/// rewriting the scheme of the typed builder's output.
fn forge_url(target_repo: &TargetRepo, forge_host: &str) -> Result<String> {
    let token = std::env::var("GITEA_TOKEN")
        .map_err(|_| Error::Config("GITEA_TOKEN not in daemon env".into()))?;
    let typed = target_repo.clone_url(forge_host);
    let path = typed
        .strip_prefix("https://")
        .and_then(|rest| rest.strip_prefix(forge_host))
        .ok_or_else(|| Error::Config("clone_url did not start with https://<forge_host>".into()))?;
    Ok(format!("https://oauth2:{token}@{forge_host}{path}"))
}

#[allow(dead_code)]
fn _used(_: BriefId, _: VersionedRef, _: ToolAllowlist, _: PermitScope) {}
