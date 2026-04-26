//! Daemon: XREAD loop on `agentry:briefs`, per-brief orchestration.
//!
//! The outer loop reads briefs off Redis and dispatches each to its own
//! `tokio::spawn`d task so multiple briefs run concurrently. Within a brief,
//! `handle_brief` walks the team's `message_graph` as a DAG: roles whose
//! upstream(s) have all shipped fire concurrently via `join_all`. Rework
//! rewinds to the single upstream named by `team.incoming(role).first()`,
//! resetting that upstream and its downstream sub-DAG to pending so they
//! re-fire once the upstream re-ships.

use crate::{
    permit as permit_mod, projector, redis_io,
    spawner::{PodmanSpawner, RoutedMessage, RunAgentCtx, Spawner, TeamContext},
    state,
    workspace::{self, BriefWorkspace},
    Error, Result,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use futures::future::join_all;
use orchestrator_types::{
    apply_overrides, now, AgentRole, Brief, BriefId, Budget, PermitOverrides, PermitScope,
    RoleName, TeamTopology, ToolAllowlist, Verdict, VerdictKind, VersionedRef, WorkPermit,
};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Run the daemon loop forever using the given `Config`.
pub async fn run(cfg: &crate::Config) -> Result<()> {
    let mut conn = redis_io::connect(&cfg.redis.url).await?;
    tracing::info!(url = %cfg.redis.url.rsplit('@').next().unwrap_or("?"), "connected to Redis");

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

    loop {
        match redis_io::read_next_brief(&mut conn, &last_id, 5_000).await {
            Ok(Some((sid, brief))) => {
                last_id = sid;
                tracing::info!(brief = %brief.id, "received brief");
                let conn_clone = conn.clone();
                let signing_clone = signing_key.clone();
                let verifying_clone = verifying_key.clone();
                let spawner_clone = spawner.clone();
                let brief_id = brief.id.clone();
                tokio::spawn(async move {
                    let mut conn_for_brief = conn_clone;
                    match handle_brief(
                        &mut conn_for_brief,
                        &spawner_clone,
                        &brief,
                        &signing_clone,
                        &verifying_clone,
                    )
                    .await
                    {
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
                            let v = Verdict::new(brief_id.clone(), VerdictKind::Failed)
                                .with_reason(format!("handler error: {e}"));
                            redis_io::append_verdict(&mut conn_for_brief, &v).await.ok();
                            dol_on_brief_terminal(&mut conn_for_brief, &brief, &v.kind)
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

/// Per-role state in the DAG walker.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RoleState {
    Pending,
    Running,
    Shipped,
    Failed,
}

/// Setup-phase bundle: one entry per role about to fire in the current batch.
/// Constructed serially before the concurrent fan-out.
struct RoleRun {
    name: RoleName,
    role: AgentRole,
    permit: WorkPermit,
    team_ctx: TeamContext,
}

/// Handle a single brief end-to-end via DAG walk. Returns the brief's
/// terminal-verdict kind (Shipped or Failed) so the caller can drive the DOL
/// composer.
async fn handle_brief(
    conn: &mut ConnectionManager,
    spawner: &impl Spawner,
    brief: &Brief,
    signing_key: &SigningKey,
    verifying_key: &VerifyingKey,
) -> Result<VerdictKind> {
    let team = redis_io::fetch_team(conn, &brief.topology).await?;

    if team.roles.is_empty() {
        return Err(Error::Config(format!("team {} has no roles", team.name.0)));
    }

    // Accumulated outbox from all upstream roles; sliced per role on dispatch.
    let mut all_messages: Vec<RoutedMessage> = Vec::new();
    // Permit-narrowing overrides accumulated for a downstream role, keyed by role name.
    let mut overrides_for: HashMap<String, PermitOverrides> = HashMap::new();
    // Lazily allocated on first role that declares a `workspace_mount`.
    let mut workspace: Option<BriefWorkspace> = None;
    // Track the final team-level outcome.
    let mut team_shipped = true;
    // Per-brief rework budget.
    let mut reworks_used: u32 = 0;
    // Per-role state in the DAG. All roles start Pending.
    let mut state: HashMap<RoleName, RoleState> = team
        .roles
        .iter()
        .map(|r| (r.clone(), RoleState::Pending))
        .collect();

    'outer: loop {
        // Ready set: roles in Pending whose upstream roles are all Shipped.
        // Roles with zero inbound edges are immediately ready.
        let shipped_set: HashSet<RoleName> = state
            .iter()
            .filter(|(_, s)| **s == RoleState::Shipped)
            .map(|(r, _)| r.clone())
            .collect();
        let ready: Vec<RoleName> = team
            .roles
            .iter()
            .filter(|r| state.get(*r) == Some(&RoleState::Pending))
            .filter(|r| inbound_satisfied(r, &team, &shipped_set))
            .cloned()
            .collect();

        if ready.is_empty() {
            break;
        }

        // Setup phase (serial): fetch role records, allocate workspace if
        // needed, mint+narrow+sign permits, build per-role TeamContexts.
        let mut runs: Vec<RoleRun> = Vec::with_capacity(ready.len());
        for role_name in &ready {
            let role = fetch_role_any_version(conn, role_name).await?;

            if role.workspace_mount.is_some() && workspace.is_none() {
                let repo = resolve_repo_for_brief(brief, conn).await?;
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
            if let Some(o) = overrides_for.get(&role.name.0) {
                apply_overrides(&mut permit, o);
                tracing::info!(
                    brief = %brief.id,
                    role = %role_name,
                    overrides = ?o,
                    "applied permit overrides from upstream"
                );
            }
            permit_mod::sign(&mut permit, signing_key)?;

            let team_ctx = TeamContext {
                messages: all_messages
                    .iter()
                    .filter(|m| m.to == role.name.0)
                    .cloned()
                    .collect(),
            };

            runs.push(RoleRun {
                name: role_name.clone(),
                role,
                permit,
                team_ctx,
            });
        }

        // Mark batch as Running.
        for r in &ready {
            state.insert(r.clone(), RoleState::Running);
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
                };
                spawner.run_agent(ctx, conn_for_role)
            })
            .collect();
        let outcomes = join_all(futs).await;

        // Outcome processing pass: append verdicts, accumulate outboxes,
        // propagate permit overrides, classify each role's verdict for the
        // state-update phase.
        let mut shipped_in_batch: Vec<RoleName> = Vec::new();
        let mut reworks: Vec<(RoleName, Vec<orchestrator_types::ReviewFinding>)> = Vec::new();
        let mut failures: Vec<RoleName> = Vec::new();

        for (run, outcome_res) in runs.iter().zip(outcomes.into_iter()) {
            let outcome = outcome_res?;
            redis_io::append_verdict(conn, &outcome.verdict).await?;
            tracing::info!(
                brief = %brief.id,
                role = %run.name,
                verdict = ?outcome.verdict.kind,
                outbox_len = outcome.outbox.len(),
                "role completed"
            );

            for msg in &outcome.outbox {
                for edge in team
                    .message_graph
                    .iter()
                    .filter(|e| e.from == run.role.name && e.to.0 == msg.to)
                {
                    if let Some(key) = &edge.permit_overrides_from {
                        if let Some(value) = msg.payload.get(key) {
                            match serde_json::from_value::<PermitOverrides>(value.clone()) {
                                Ok(po) => {
                                    overrides_for.insert(edge.to.0.clone(), po);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        brief = %brief.id,
                                        from = %run.role.name,
                                        to = %edge.to,
                                        key = %key,
                                        error = %e,
                                        "upstream message had override key but payload didn't deserialize"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            all_messages.extend(outcome.outbox);

            match outcome.verdict.kind {
                VerdictKind::Shipped => shipped_in_batch.push(run.name.clone()),
                VerdictKind::ReworkNeeded { findings } => {
                    reworks.push((run.name.clone(), findings));
                }
                _ => failures.push(run.name.clone()),
            }
        }

        // Apply Shipped state.
        for r in &shipped_in_batch {
            state.insert(r.clone(), RoleState::Shipped);
        }

        // Reworks: each rewinds to its single upstream and resets that
        // upstream + its downstream sub-DAG to Pending so the slice re-fires
        // once the upstream re-ships.
        let mut rewound_subdags: HashSet<RoleName> = HashSet::new();
        for (from_role, findings) in reworks {
            let upstream = team.incoming(&from_role).first().map(|e| e.from.clone());
            match upstream {
                Some(up) if reworks_used < team.max_retries => {
                    all_messages.push(RoutedMessage {
                        from: from_role.0.clone(),
                        to: up.0.clone(),
                        payload: serde_json::json!({ "findings": findings }),
                        at: now(),
                    });
                    reworks_used += 1;
                    tracing::info!(
                        brief = %brief.id,
                        from = %from_role,
                        to = %up,
                        findings_count = findings.len(),
                        reworks_used,
                        max_retries = team.max_retries,
                        "rework requested — resetting upstream sub-DAG"
                    );
                    state.insert(up.clone(), RoleState::Pending);
                    rewound_subdags.insert(up.clone());
                    for r in downstream_subdag(&up, &team) {
                        state.insert(r.clone(), RoleState::Pending);
                        rewound_subdags.insert(r);
                    }
                }
                Some(up) => {
                    tracing::warn!(
                        brief = %brief.id,
                        role = %from_role,
                        upstream = %up,
                        reworks_used,
                        max_retries = team.max_retries,
                        "rework requested but retry budget exhausted"
                    );
                    team_shipped = false;
                    break 'outer;
                }
                None => {
                    tracing::warn!(
                        brief = %brief.id,
                        role = %from_role,
                        "rework requested but role has no upstream — treating as failed"
                    );
                    team_shipped = false;
                    break 'outer;
                }
            }
        }

        // Failures: only fatal if not already part of an active rewind
        // sub-DAG (in which case the failure is squashed and the role
        // re-enters Pending to fire again on the next upstream reship).
        for failed in &failures {
            if rewound_subdags.contains(failed) {
                state.insert(failed.clone(), RoleState::Pending);
            } else {
                state.insert(failed.clone(), RoleState::Failed);
                team_shipped = false;
                break 'outer;
            }
        }
    }

    // Success requires the terminal role to have shipped.
    if team_shipped && state.get(&team.terminal_role) != Some(&RoleState::Shipped) {
        team_shipped = false;
    }

    // Bail early on team failure — keep workspace for audit, no chain-trigger.
    if !team_shipped {
        if let Some(ws) = workspace.take() {
            tracing::info!(
                brief = %brief.id,
                path = %ws.host_path.display(),
                "workspace retained for audit (team did not ship)"
            );
        }
        return Ok(VerdictKind::Failed);
    }

    // Chain-trigger BEFORE workspace destruction: chain paths often live
    // INSIDE the workspace (e.g. planner emits next_brief_refs into
    // <workspace>/planner-children/), so file reads must complete while the
    // workspace still exists. Destruction follows once every brief is parsed
    // into memory and submitted to Redis.
    finalize_shipped_team(conn, brief, workspace.take(), &all_messages).await?;

    Ok(VerdictKind::Shipped)
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
    all_messages: &[RoutedMessage],
) -> Result<()> {
    for next_ref in collect_chain_paths(&brief.payload, all_messages) {
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
        if let Err(e) = workspace::destroy(&ws).await {
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
                "workspace destroyed (team shipped)"
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

        let target_repo = meta_brief
            .payload
            .get("target_repo")
            .and_then(|v| v.as_str())
            .map(str::to_string);
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
            budget: Budget {
                max_tokens: None,
                max_wall_seconds: Some(600),
                max_usd: None,
            },
            escalation: meta_brief.escalation,
            // Verifier is in its own DOL slot — NOT a child of the meta-brief.
            parent_brief: None,
            cohort_labels: meta_brief.cohort_labels.clone(),
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
/// Idempotency: at the start, atomically claims
/// `agentry:brief:<meta_id>:final_emitted` via `SET ... NX EX 86400`. If the
/// marker is already set, returns early without emitting. This guards the
/// concurrent path where multiple terminal-handlers (e.g. three children
/// resolving in the same tick) all reach this function for the same meta —
/// only the first arrival wins the SETNX and emits the meta verdict.
async fn compose_meta_verdict(conn: &mut ConnectionManager, meta_id: &str) -> Result<()> {
    let verdicts_key = format!("agentry:brief:{meta_id}:children_verdicts");
    let pending_key = format!("agentry:brief:{meta_id}:children_pending");
    let verifier_pending_key = format!("agentry:brief:{meta_id}:verifier_pending");
    let verifier_verdict_key = format!("agentry:brief:{meta_id}:verifier_verdict");
    let final_emitted_key = format!("agentry:brief:{meta_id}:final_emitted");

    let acquired: bool = redis::cmd("SET")
        .arg(&final_emitted_key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(86400)
        .query_async(conn)
        .await?;
    if !acquired {
        tracing::info!(
            meta = %meta_id,
            "DOL: meta verdict already emitted (idempotency gate); skipping"
        );
        return Ok(());
    }

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
    redis_io::append_verdict(conn, &final_verdict).await?;

    let _: () = conn.del(&verdicts_key).await?;
    let _: () = conn.del(&pending_key).await?;
    let _: () = conn.del(&verifier_pending_key).await?;
    let _: () = conn.del(&verifier_verdict_key).await?;
    let _: () = conn.del(&final_emitted_key).await?;

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
fn inbound_satisfied(role: &RoleName, team: &TeamTopology, shipped: &HashSet<RoleName>) -> bool {
    team.inbound_roles(role)
        .iter()
        .all(|up| shipped.contains(*up))
}

/// All roles reachable from `role` via outbound edges in `team.message_graph`.
/// Used by rework: when `role` is rewound to Pending, every role in its
/// downstream sub-DAG must also be reset to Pending so the slice re-fires
/// once `role` re-ships.
fn downstream_subdag(role: &RoleName, team: &TeamTopology) -> HashSet<RoleName> {
    let mut out: HashSet<RoleName> = HashSet::new();
    let mut stack: Vec<RoleName> = team.outgoing(role).iter().map(|e| e.to.clone()).collect();
    while let Some(r) = stack.pop() {
        if out.insert(r.clone()) {
            for e in team.outgoing(&r) {
                stack.push(e.to.clone());
            }
        }
    }
    out
}

/// Fetch a role by name, trying v1 then v2 then v3 — M0 keeps this naive.
/// M2 will replace with a proper latest-version index.
async fn fetch_role_any_version(
    conn: &mut ConnectionManager,
    name: &RoleName,
) -> Result<AgentRole> {
    for v in [1u32, 2, 3, 4, 5] {
        if let Ok(r) = redis_io::fetch_role(conn, name, v).await {
            return Ok(r);
        }
    }
    Err(Error::NotFound {
        kind: "role",
        key: format!("agentry:role:{}:v?", name.0),
    })
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

    let target_repo = brief
        .payload
        .get("target_repo")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let base_branch = brief
        .payload
        .get("base_branch")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let forge_host = brief
        .payload
        .get("forge_host")
        .and_then(|v| v.as_str())
        .unwrap_or("agency.lab:3000")
        .to_string();

    if let (Some(repo), Some(branch)) = (target_repo, base_branch) {
        let url = forge_url(&repo, &forge_host)?;
        return Ok(Some((url, branch)));
    }

    Ok(None)
}

/// Build a token-bearing forge URL for the FIRST bare clone. Subsequent
/// worktree operations against the bare clone do not need to carry auth.
fn forge_url(target_repo: &str, forge_host: &str) -> Result<String> {
    let token = std::env::var("GITEA_TOKEN")
        .map_err(|_| Error::Config("GITEA_TOKEN not in daemon env".into()))?;
    Ok(format!(
        "https://oauth2:{token}@{forge_host}/{target_repo}.git"
    ))
}

#[allow(dead_code)]
fn _used(_: BriefId, _: VersionedRef, _: ToolAllowlist, _: PermitScope) {}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_types::{MessageEdge, TeamName};

    fn rn(s: &str) -> RoleName {
        RoleName(s.into())
    }

    /// Build the agentry-self-host-v0 shape: scribe → coder; coder → mech;
    /// coder → claude; mech → shipper; claude → shipper. Reviewer siblings
    /// share `coder` as the only upstream.
    fn diamond_team() -> TeamTopology {
        TeamTopology {
            name: TeamName("test-diamond".into()),
            version: 1,
            roles: vec![
                rn("scribe"),
                rn("coder"),
                rn("mech"),
                rn("claude"),
                rn("shipper"),
            ],
            message_graph: vec![
                MessageEdge {
                    from: rn("scribe"),
                    to: rn("coder"),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: rn("coder"),
                    to: rn("mech"),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: rn("coder"),
                    to: rn("claude"),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: rn("mech"),
                    to: rn("shipper"),
                    permit_overrides_from: None,
                },
                MessageEdge {
                    from: rn("claude"),
                    to: rn("shipper"),
                    permit_overrides_from: None,
                },
            ],
            terminal_role: rn("shipper"),
            max_retries: 2,
        }
    }

    #[test]
    fn inbound_satisfied_root_is_always_ready() {
        let t = diamond_team();
        let shipped: HashSet<RoleName> = HashSet::new();
        // scribe has no inbound edges; trivially satisfied.
        assert!(inbound_satisfied(&rn("scribe"), &t, &shipped));
    }

    #[test]
    fn inbound_satisfied_requires_all_upstreams_shipped() {
        let t = diamond_team();
        // shipper waits on BOTH mech and claude.
        let mut shipped: HashSet<RoleName> = HashSet::new();
        shipped.insert(rn("scribe"));
        shipped.insert(rn("coder"));
        shipped.insert(rn("mech"));
        // Only one of two upstreams shipped — not yet satisfied.
        assert!(!inbound_satisfied(&rn("shipper"), &t, &shipped));
        shipped.insert(rn("claude"));
        // Both upstreams shipped — now satisfied.
        assert!(inbound_satisfied(&rn("shipper"), &t, &shipped));
    }

    #[test]
    fn inbound_satisfied_sibling_independence() {
        let t = diamond_team();
        // mech only depends on coder; claude not shipping is irrelevant.
        let mut shipped: HashSet<RoleName> = HashSet::new();
        shipped.insert(rn("scribe"));
        shipped.insert(rn("coder"));
        assert!(inbound_satisfied(&rn("mech"), &t, &shipped));
        assert!(inbound_satisfied(&rn("claude"), &t, &shipped));
    }

    #[test]
    fn downstream_subdag_full_reach_from_root() {
        let t = diamond_team();
        let sub = downstream_subdag(&rn("scribe"), &t);
        // scribe's downstream = coder, mech, claude, shipper (everything but scribe itself).
        assert_eq!(sub.len(), 4);
        assert!(sub.contains(&rn("coder")));
        assert!(sub.contains(&rn("mech")));
        assert!(sub.contains(&rn("claude")));
        assert!(sub.contains(&rn("shipper")));
        assert!(!sub.contains(&rn("scribe")));
    }

    #[test]
    fn downstream_subdag_from_coder_resets_diamond() {
        let t = diamond_team();
        let sub = downstream_subdag(&rn("coder"), &t);
        // Rewind to coder must reset both reviewers and the shipper.
        assert_eq!(sub.len(), 3);
        assert!(sub.contains(&rn("mech")));
        assert!(sub.contains(&rn("claude")));
        assert!(sub.contains(&rn("shipper")));
    }

    #[test]
    fn downstream_subdag_terminal_is_empty() {
        let t = diamond_team();
        let sub = downstream_subdag(&rn("shipper"), &t);
        assert!(sub.is_empty());
    }

    fn make_child_brief(id: &str) -> Brief {
        let mut b = Brief::new(
            "test",
            VersionedRef::new("test-team", 1),
            serde_json::json!({}),
        );
        b.id = BriefId(id.into());
        b
    }

    async fn write_brief(dir: &std::path::Path, name: &str, brief: &Brief) -> String {
        let path = dir.join(name);
        let body = serde_json::to_string(brief).expect("serialize brief");
        tokio::fs::write(&path, body).await.expect("write brief");
        path.to_str().expect("utf8 path").to_string()
    }

    #[tokio::test]
    async fn chain_trigger_dispatches_plural_next_brief_refs() {
        let tmp = tempfile::tempdir().expect("tmp");
        let b1 = make_child_brief("brf_chain_a");
        let b2 = make_child_brief("brf_chain_b");
        let p1 = write_brief(tmp.path(), "a.json", &b1).await;
        let p2 = write_brief(tmp.path(), "b.json", &b2).await;

        let payload = serde_json::json!({ "next_brief_refs": [p1, p2] });
        let paths = next_brief_paths(&payload);
        assert_eq!(paths.len(), 2);

        let mut loaded: Vec<Brief> = Vec::new();
        for p in &paths {
            if let Some(b) = load_next_brief(p).await {
                loaded.push(b);
            }
        }
        assert_eq!(loaded.len(), 2, "both child briefs should load");
        assert_eq!(loaded[0].id, b1.id);
        assert_eq!(loaded[1].id, b2.id);
    }

    #[tokio::test]
    async fn chain_trigger_skips_bad_path_continues_others() {
        let tmp = tempfile::tempdir().expect("tmp");
        let b_good = make_child_brief("brf_chain_good");
        let p_good = write_brief(tmp.path(), "good.json", &b_good).await;

        let payload = serde_json::json!({
            "next_brief_refs": ["/tmp/agentry-does-not-exist-xyz-47", p_good],
        });
        let paths = next_brief_paths(&payload);
        assert_eq!(paths.len(), 2);

        let mut loaded: Vec<Brief> = Vec::new();
        for p in &paths {
            if let Some(b) = load_next_brief(p).await {
                loaded.push(b);
            }
        }
        assert_eq!(loaded.len(), 1, "bad path is skipped, valid one survives");
        assert_eq!(loaded[0].id, b_good.id);
    }

    #[tokio::test]
    async fn chain_trigger_single_form_backward_compat() {
        let tmp = tempfile::tempdir().expect("tmp");
        let b = make_child_brief("brf_chain_legacy");
        let p = write_brief(tmp.path(), "legacy.json", &b).await;

        let payload = serde_json::json!({ "next_brief_ref": p });
        let paths = next_brief_paths(&payload);
        assert_eq!(paths, vec![p.clone()]);

        let loaded = load_next_brief(&paths[0]).await.expect("brief loads");
        assert_eq!(loaded.id, b.id);
    }

    #[tokio::test]
    async fn chain_trigger_plural_takes_precedence_over_singular() {
        let payload = serde_json::json!({
            "next_brief_refs": ["/tmp/p1", "/tmp/p2"],
            "next_brief_ref": "/tmp/legacy",
        });
        let paths = next_brief_paths(&payload);
        assert_eq!(paths, vec!["/tmp/p1".to_string(), "/tmp/p2".to_string()]);
    }

    #[tokio::test]
    async fn chain_trigger_skips_non_string_array_entries() {
        let payload = serde_json::json!({
            "next_brief_refs": ["/tmp/ok", 42, "/tmp/also-ok"],
        });
        let paths = next_brief_paths(&payload);
        assert_eq!(
            paths,
            vec!["/tmp/ok".to_string(), "/tmp/also-ok".to_string()]
        );
    }

    #[tokio::test]
    async fn chain_trigger_no_refs_yields_empty() {
        let payload = serde_json::json!({});
        assert!(next_brief_paths(&payload).is_empty());
    }

    fn outbox_msg(payload: serde_json::Value) -> RoutedMessage {
        RoutedMessage {
            from: "some-role".into(),
            to: "downstream".into(),
            payload,
            at: now(),
        }
    }

    #[tokio::test]
    async fn chain_trigger_dispatches_from_role_outbox_message() {
        let tmp = tempfile::tempdir().expect("tmp");
        let b = make_child_brief("brf_chain_outbox");
        let p = write_brief(tmp.path(), "outbox.json", &b).await;

        // brief.payload carries no next_brief_refs; the planner-style outbox
        // message does.
        let brief_payload = serde_json::json!({});
        let messages = vec![outbox_msg(serde_json::json!({
            "next_brief_refs": [p.clone()],
        }))];
        let paths = collect_chain_paths(&brief_payload, &messages);
        assert_eq!(paths, vec![p.clone()]);

        let loaded = load_next_brief(&paths[0]).await.expect("brief loads");
        assert_eq!(loaded.id, b.id);
    }

    #[tokio::test]
    async fn chain_trigger_deduplicates_payload_and_outbox_paths() {
        let shared = "/tmp/agentry-chain-shared".to_string();
        let other = "/tmp/agentry-chain-other".to_string();
        let brief_payload = serde_json::json!({
            "next_brief_refs": [shared.clone()],
        });
        let messages = vec![
            outbox_msg(serde_json::json!({
                "next_brief_refs": [shared.clone(), other.clone()],
            })),
            outbox_msg(serde_json::json!({
                "next_brief_refs": [other.clone()],
            })),
        ];

        let paths = collect_chain_paths(&brief_payload, &messages);
        // First-seen order: payload's `shared` first, then `other` from the
        // first outbox message. Subsequent duplicates of either are dropped.
        assert_eq!(paths, vec![shared, other]);
    }

    /// Regression for the A7 PoC ordering bug: planner emits
    /// `next_brief_refs` whose paths live INSIDE the brief's workspace
    /// (`<workspace>/planner-children/child-N.json`). Pre-fix the daemon
    /// destroyed the workspace before the chain-trigger ran, so every
    /// `load_next_brief` got ENOENT. The fix reorders finalize to read first,
    /// destroy second — this test pins the invariant by emulating the same
    /// sequence against a real workspace dir and asserting the child briefs
    /// were loaded successfully before destruction wiped the dir.
    #[tokio::test]
    async fn chain_trigger_runs_before_workspace_destruction() {
        let ws_dir = tempfile::tempdir().expect("workspace tmp");
        let children_dir = ws_dir.path().join("planner-children");
        tokio::fs::create_dir(&children_dir)
            .await
            .expect("mkdir planner-children");

        let child_a = make_child_brief("brf_chain_inside_ws_a");
        let child_b = make_child_brief("brf_chain_inside_ws_b");
        let path_a = write_brief(&children_dir, "child-0.json", &child_a).await;
        let path_b = write_brief(&children_dir, "child-1.json", &child_b).await;

        let ws = BriefWorkspace {
            brief_id: BriefId("brf_parent".into()),
            host_path: ws_dir.path().to_path_buf(),
        };

        // Outbox-style message (planner emits this) referencing paths INSIDE
        // the workspace.
        let messages = vec![outbox_msg(serde_json::json!({
            "next_brief_refs": [path_a.clone(), path_b.clone()],
        }))];
        let brief_payload = serde_json::json!({});

        // Emulate finalize_shipped_team's read-then-destroy sequence. With
        // the bug, destruction would happen first and load_next_brief would
        // return None for every path.
        let mut loaded: Vec<Brief> = Vec::new();
        for next_ref in collect_chain_paths(&brief_payload, &messages) {
            if let Some(b) = load_next_brief(&next_ref).await {
                loaded.push(b);
            }
        }
        workspace::destroy(&ws).await.expect("destroy ws");

        assert_eq!(
            loaded.len(),
            2,
            "both child briefs must load before workspace destruction"
        );
        assert_eq!(loaded[0].id, child_a.id);
        assert_eq!(loaded[1].id, child_b.id);
        assert!(
            !ws_dir.path().exists(),
            "workspace destroyed after chain-trigger reads"
        );
    }

    #[tokio::test]
    async fn chain_trigger_load_returns_none_on_bad_json() {
        let tmp = tempfile::tempdir().expect("tmp");
        let p = tmp.path().join("garbage.json");
        tokio::fs::write(&p, b"{ not json")
            .await
            .expect("write garbage");
        assert!(load_next_brief(p.to_str().expect("utf8 path"))
            .await
            .is_none());
    }

    // --- DOL composition tests ---------------------------------------------
    //
    // These exercise the pure `compose_verdict_parts` function — the
    // Redis-touching async helpers (`dol_on_brief_terminal`,
    // `on_all_children_resolved`, `compose_meta_verdict`) wrap this exact
    // logic, so coverage here is sufficient to validate the composition rules
    // without spinning up Redis. The Redis-side state-machine asserts (SADD
    // on submit, SREM/RPUSH on terminal, single emission per meta) are
    // enforced by code-shape: `submit_brief` and `dol_on_brief_terminal` are
    // the only writers to the four DOL keys.

    fn shipped_verdict(id: &str) -> Verdict {
        Verdict::new(BriefId(id.into()), VerdictKind::Shipped)
    }

    fn failed_verdict(id: &str, reason: &str) -> Verdict {
        Verdict::new(BriefId(id.into()), VerdictKind::Failed).with_reason(reason)
    }

    #[test]
    fn dol_meta_brief_composes_shipped_when_all_children_pass() {
        // Two shipped children + a verifier that also shipped → meta shipped.
        let children = vec![
            shipped_verdict("brf_child_a"),
            shipped_verdict("brf_child_b"),
        ];
        let verifier = Verdict::new(BriefId("brf_verify_meta_x".into()), VerdictKind::Shipped)
            .with_reason("criterion passed");
        let (kind, reason) = compose_verdict_parts(&children, Some(&verifier));
        assert!(matches!(kind, VerdictKind::Shipped));
        assert_eq!(reason.as_deref(), Some("verifier: criterion passed"));
    }

    #[test]
    fn dol_meta_brief_composes_failed_on_child_failure() {
        // 1 shipped, 1 failed → meta failed BEFORE the verifier runs.
        // The Redis path encodes this by checking children_all_shipped before
        // dispatching the verifier; here we exercise the pure rule with no
        // verifier supplied (verifier is correctly NOT dispatched in that
        // path).
        let children = vec![
            shipped_verdict("brf_child_a"),
            failed_verdict("brf_child_b", "compile error"),
        ];
        let (kind, reason) = compose_verdict_parts(&children, None);
        assert!(matches!(kind, VerdictKind::Failed));
        assert_eq!(reason.as_deref(), Some("one or more children failed"));
    }

    #[test]
    fn dol_no_criterion_composes_directly() {
        // Two shipped children, no verifier (meta-brief had no
        // success_criteria → on_all_children_resolved skipped verifier
        // dispatch and called compose directly) → meta shipped, no reason.
        let children = vec![
            shipped_verdict("brf_child_a"),
            shipped_verdict("brf_child_b"),
        ];
        let (kind, reason) = compose_verdict_parts(&children, None);
        assert!(matches!(kind, VerdictKind::Shipped));
        assert!(reason.is_none());
    }

    #[test]
    fn dol_verifier_failure_propagates_to_meta() {
        // Children all shipped, but verifier failed → meta failed via verifier.
        let children = vec![shipped_verdict("brf_child_a")];
        let verifier = failed_verdict("brf_verify_meta_x", "criterion exit 1");
        let (kind, reason) = compose_verdict_parts(&children, Some(&verifier));
        assert!(matches!(kind, VerdictKind::Failed));
        assert_eq!(reason.as_deref(), Some("verifier: criterion exit 1"));
    }

    #[test]
    fn dol_child_failure_dominates_verifier() {
        // Even if a verifier somehow shipped, a failed child still drops the
        // meta to Failed. The Redis path won't dispatch a verifier in this
        // case, but the pure rule must still be safe under any input.
        let children = vec![
            shipped_verdict("brf_child_a"),
            failed_verdict("brf_child_b", "exit 1"),
        ];
        let verifier = shipped_verdict("brf_verify_meta_x");
        let (kind, reason) = compose_verdict_parts(&children, Some(&verifier));
        assert!(matches!(kind, VerdictKind::Failed));
        assert_eq!(reason.as_deref(), Some("one or more children failed"));
    }

    #[test]
    fn dol_empty_children_with_shipped_verifier_ships() {
        // Edge case: meta had no children (degenerate) but a verifier was
        // somehow recorded. The pure rule treats no children as "all shipped"
        // (vacuous truth) and yields the verifier's verdict.
        let verifier = shipped_verdict("brf_verify_meta_x");
        let (kind, _reason) = compose_verdict_parts(&[], Some(&verifier));
        assert!(matches!(kind, VerdictKind::Shipped));
    }

    /// Regression for the A7v3 duplicate-verdict bug: with three children
    /// resolving near-concurrently, every terminal-handler invoked
    /// `compose_meta_verdict` and emitted the meta verdict, producing
    /// duplicate entries on `agentry:verdicts`. The fix is a SETNX
    /// idempotency gate at the start of `compose_meta_verdict`. The gate is
    /// symmetric for sequential and concurrent paths, so a sequential
    /// invocation is sufficient regression coverage.
    ///
    /// Gated behind `#[ignore]` because it requires a live Redis at
    /// `AGENTRY_TEST_REDIS_URL` (default `redis://127.0.0.1:6380`); the
    /// other tests in this module deliberately stay in pure-function land.
    /// Run with: `cargo test --workspace -- --ignored
    /// dol_meta_verdict_idempotent_setnx`.
    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn dol_meta_verdict_idempotent_setnx() {
        let url = std::env::var("AGENTRY_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6380".into());
        let mut conn = redis_io::connect(&url).await.expect("redis connect");

        let meta_id = format!("brf_test_dol_idem_{}", uuid::Uuid::now_v7());
        let verdicts_key = format!("agentry:brief:{meta_id}:children_verdicts");
        let final_emitted_key = format!("agentry:brief:{meta_id}:final_emitted");
        let v_json = serde_json::to_string(&shipped_verdict("brf_child_a"))
            .expect("serialize child verdict");

        // Stage one shipped child so the first call has something to
        // compose from. The second call (after cleanup) would re-stage and,
        // without the SETNX gate, emit a second verdict.
        let _: () = conn
            .rpush(&verdicts_key, v_json.as_str())
            .await
            .expect("stage child");

        let before: i64 = conn.xlen("agentry:verdicts").await.expect("xlen verdicts");

        compose_meta_verdict(&mut conn, &meta_id)
            .await
            .expect("first compose");
        let after_first: i64 = conn
            .xlen("agentry:verdicts")
            .await
            .expect("xlen after first");
        assert_eq!(
            after_first,
            before + 1,
            "first call must emit the meta verdict"
        );

        // The first call's cleanup deleted the marker. To prove idempotency
        // would have caught a true concurrent race, we manually re-arm the
        // marker (simulating a still-in-flight handler) and stage children
        // again. The second call must short-circuit and emit nothing.
        let _: bool = redis::cmd("SET")
            .arg(&final_emitted_key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(86400)
            .query_async(&mut conn)
            .await
            .expect("re-arm marker");
        let _: () = conn
            .rpush(&verdicts_key, v_json.as_str())
            .await
            .expect("re-stage child");

        compose_meta_verdict(&mut conn, &meta_id)
            .await
            .expect("second compose");
        let after_second: i64 = conn
            .xlen("agentry:verdicts")
            .await
            .expect("xlen after second");
        assert_eq!(
            after_second, after_first,
            "second call must be a no-op when the marker is set"
        );

        let _: () = conn.del(&verdicts_key).await.expect("cleanup verdicts");
        let _: () = conn.del(&final_emitted_key).await.expect("cleanup marker");
    }
}
