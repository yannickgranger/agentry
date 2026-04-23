//! Seed the Redis registry with the echo-team and echo-agent role.
//!
//! Idempotent: overwrites existing records with current definitions.

use crate::{Result, redis_io};
use orchestrator_types::{
    AgentRole, MessageEdge, PermitScope, RoleName, SubstrateClass, TeamName, TeamTopology,
    ToolAllowlist,
};

/// Seed the M0 registry.
pub async fn seed_m0() -> Result<()> {
    let mut conn = redis_io::connect().await?;

    let echo = AgentRole {
        name: RoleName("echo-agent".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "localhost/agentry/echo-agent:v1".into(),
        substrate_class: SubstrateClass::Podman,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
    };

    let team = TeamTopology {
        name: TeamName("echo-team".into()),
        version: 1,
        roles: vec![echo.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: echo.name.clone(),
        max_retries: 0,
    };

    redis_io::save_role(&mut conn, &echo).await?;
    redis_io::save_team(&mut conn, &team).await?;

    tracing::info!("seeded: role echo-agent v1, team echo-team v1");
    Ok(())
}
