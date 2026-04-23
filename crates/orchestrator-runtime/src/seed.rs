//! Seed the Redis registry with the echo-team and echo-agent role.
//!
//! Idempotent: overwrites existing records with current definitions.

use crate::{Result, redis_io};
use orchestrator_types::{
    AgentRole, MessageEdge, PermitScope, RoleName, SubstrateClass, TeamName, TeamTopology,
    ToolAllowlist,
};

/// Seed the M0/M3 registry.
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

    let echo_team = TeamTopology {
        name: TeamName("echo-team".into()),
        version: 1,
        roles: vec![echo.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: echo.name.clone(),
        max_retries: 0,
    };

    // M3: naughty agent — allowlist is `[read]`; the container will emit a
    // `write` tool_call which the broker must block. Verify expects a
    // permit_violation verdict.
    let naughty = AgentRole {
        name: RoleName("naughty-agent".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "localhost/agentry/naughty-agent:v1".into(),
        substrate_class: SubstrateClass::Podman,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec!["read".into()]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
    };

    let naughty_team = TeamTopology {
        name: TeamName("naughty-team".into()),
        version: 1,
        roles: vec![naughty.name.clone()],
        message_graph: Vec::<MessageEdge>::new(),
        terminal_role: naughty.name.clone(),
        max_retries: 0,
    };

    redis_io::save_role(&mut conn, &echo).await?;
    redis_io::save_team(&mut conn, &echo_team).await?;
    redis_io::save_role(&mut conn, &naughty).await?;
    redis_io::save_team(&mut conn, &naughty_team).await?;

    // M4: speaker + listener, for inter-role message routing.
    let speaker = AgentRole {
        name: RoleName("speaker-agent".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "localhost/agentry/speaker-agent:v1".into(),
        substrate_class: SubstrateClass::Podman,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
    };
    let listener = AgentRole {
        name: RoleName("listener-agent".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "localhost/agentry/listener-agent:v1".into(),
        substrate_class: SubstrateClass::Podman,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
    };
    let speaker_listener_team = TeamTopology {
        name: TeamName("speaker-listener-team".into()),
        version: 1,
        roles: vec![speaker.name.clone(), listener.name.clone()],
        message_graph: vec![orchestrator_types::MessageEdge {
            from: speaker.name.clone(),
            to: listener.name.clone(),
            permit_overrides_from: None,
        }],
        terminal_role: listener.name.clone(),
        max_retries: 0,
    };

    redis_io::save_role(&mut conn, &speaker).await?;
    redis_io::save_role(&mut conn, &listener).await?;
    redis_io::save_team(&mut conn, &speaker_listener_team).await?;

    tracing::info!(
        "seeded: roles [echo-agent, naughty-agent, speaker-agent, listener-agent] v1, teams [echo-team, naughty-team, speaker-listener-team] v1"
    );
    Ok(())
}
