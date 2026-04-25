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
    permit as permit_mod, redis_io,
    spawner::{PodmanSpawner, RoutedMessage, RunAgentCtx, Spawner, TeamContext},
    workspace::{self, BriefWorkspace},
    Error, Result,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use futures::future::join_all;
use orchestrator_types::{
    apply_overrides, now, AgentRole, Brief, BriefId, PermitOverrides, PermitScope, RoleName,
    TeamTopology, ToolAllowlist, Verdict, VerdictKind, VersionedRef, WorkPermit,
};
use redis::aio::ConnectionManager;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Run the daemon loop forever using the given `Config`.
pub async fn run(cfg: &crate::Config) -> Result<()> {
    let mut conn = redis_io::connect(&cfg.redis.url).await?;
    tracing::info!(url = %cfg.redis.url.rsplit('@').next().unwrap_or("?"), "connected to Redis");

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
                    if let Err(e) = handle_brief(
                        &mut conn_for_brief,
                        &spawner_clone,
                        &brief,
                        &signing_clone,
                        &verifying_clone,
                    )
                    .await
                    {
                        tracing::error!(brief = %brief_id, error = %e, "brief handling failed");
                        let v = Verdict::new(brief_id.clone(), VerdictKind::Failed)
                            .with_reason(format!("handler error: {e}"));
                        redis_io::append_verdict(&mut conn_for_brief, &v).await.ok();
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

/// Handle a single brief end-to-end via DAG walk.
async fn handle_brief(
    conn: &mut ConnectionManager,
    spawner: &impl Spawner,
    brief: &Brief,
    signing_key: &SigningKey,
    verifying_key: &VerifyingKey,
) -> Result<()> {
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

    // Workspace teardown: destroy on Shipped, retain otherwise for audit.
    if let Some(ws) = workspace.take() {
        if team_shipped {
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
        } else {
            tracing::info!(
                brief = %brief.id,
                path = %ws.host_path.display(),
                "workspace retained for audit (team did not ship)"
            );
        }
    }

    if !team_shipped {
        return Ok(());
    }

    // Chain trigger: if every role shipped AND the brief's payload names
    // one or more next briefs (each an absolute file path containing another
    // Brief JSON), submit each to the queue. Plural form `next_brief_refs`
    // (JSON array of strings) takes precedence; singular `next_brief_ref`
    // is preserved for backward compatibility.
    for next_ref in next_brief_paths(&brief.payload) {
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

    Ok(())
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
}
