//! Daemon: XREAD loop on `agentry:briefs`, per-brief orchestration.
//!
//! M0 scope:
//!   - Single-role teams only (echo-team).
//!   - Spawn → run → verdict → next.
//!   - Message routing between roles: M4+.

use crate::{
    Error, Result, permit as permit_mod, redis_io,
    spawner::{PodmanSpawner, RoutedMessage, Spawner, TeamContext},
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use orchestrator_types::{
    AgentRole, Brief, BriefId, PermitScope, RoleName, ToolAllowlist, Verdict, VerdictKind,
    VersionedRef, WorkPermit, now,
};
use redis::aio::ConnectionManager;
use std::sync::Arc;

/// Run the daemon loop forever.
pub async fn run() -> Result<()> {
    let mut conn = redis_io::connect().await?;
    tracing::info!("connected to Redis");

    // Load signing key. Fail loudly if missing.
    let key_path = permit_mod::key_path();
    if !key_path.exists() {
        return Err(Error::Config(format!(
            "signing key not found at {}. Run `orchestrator key-gen` first.",
            key_path.display()
        )));
    }
    let signing_key = Arc::new(permit_mod::load_signing_key(&key_path)?);
    let verifying_key = Arc::new(signing_key.verifying_key());
    tracing::info!(key = %key_path.display(), "signing key loaded");

    let spawner = PodmanSpawner::new();
    let mut last_id = "$".to_string(); // only new briefs

    loop {
        match redis_io::read_next_brief(&mut conn, &last_id, 5_000).await {
            Ok(Some((sid, brief))) => {
                last_id = sid;
                tracing::info!(brief = %brief.id, "received brief");
                if let Err(e) =
                    handle_brief(&mut conn, &spawner, &brief, &signing_key, &verifying_key).await
                {
                    tracing::error!(brief = %brief.id, error = %e, "brief handling failed");
                    let v = Verdict::new(brief.id.clone(), VerdictKind::Failed)
                        .with_reason(format!("handler error: {e}"));
                    redis_io::append_verdict(&mut conn, &v).await.ok();
                }
            }
            Ok(None) => {
                // Block timeout with no entries; loop.
            }
            Err(e) => {
                tracing::error!(error = %e, "XREAD failed; backing off");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

/// Handle a single brief end-to-end.
async fn handle_brief(
    conn: &mut ConnectionManager,
    spawner: &impl Spawner,
    brief: &Brief,
    signing_key: &SigningKey,
    verifying_key: &VerifyingKey,
) -> Result<()> {
    let team = redis_io::fetch_team(conn, &brief.topology).await?;

    if team.roles.is_empty() {
        return Err(Error::Config(format!(
            "team {} has no roles",
            team.name.0
        )));
    }

    // Accumulated outbox from all upstream roles; sliced per role on dispatch.
    let mut all_messages: Vec<RoutedMessage> = Vec::new();

    for role_name in &team.roles {
        let role = fetch_role_any_version(conn, role_name).await?;
        let mut permit = mint_permit(brief, &role)?;
        permit_mod::sign(&mut permit, signing_key)?;

        // Messages addressed to this role from any prior step.
        let team_ctx = TeamContext {
            messages: all_messages
                .iter()
                .filter(|m| m.to == role.name.0)
                .cloned()
                .collect(),
        };

        let outcome = spawner
            .run_agent(brief, &role, &permit, verifying_key, &team_ctx, conn)
            .await?;
        redis_io::append_verdict(conn, &outcome.verdict).await?;
        tracing::info!(
            brief = %brief.id,
            role = %role_name,
            verdict = ?outcome.verdict.kind,
            outbox_len = outcome.outbox.len(),
            "role completed"
        );
        all_messages.extend(outcome.outbox);

        if !matches!(outcome.verdict.kind, VerdictKind::Shipped) {
            // On non-shipped verdict, stop the team run.
            return Ok(());
        }
    }
    Ok(())
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

/// Mint an unsigned permit for M0. Signing lands in M3.
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

#[allow(dead_code)]
fn _used(_: BriefId, _: VersionedRef, _: ToolAllowlist, _: PermitScope) {}
