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
        passthru_env: vec![],
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
        passthru_env: vec![],
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
        passthru_env: vec![],
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
        passthru_env: vec![],
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

    // M5a: grok-echo — cheap xAI agent. Passthru XAI_API_KEY from orchestratord env.
    let grok_echo = AgentRole {
        name: RoleName("grok-echo".into()),
        version: 1,
        model: Some("grok-4-fast".into()),
        system_prompt: None,
        image: "localhost/agentry/grok-agent:v1".into(),
        substrate_class: SubstrateClass::Podman,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:allow:api.x.ai".into()]),
        passthru_env: vec!["XAI_API_KEY".into()],
    };
    let grok_team = TeamTopology {
        name: TeamName("grok-echo-team".into()),
        version: 1,
        roles: vec![grok_echo.name.clone()],
        message_graph: vec![],
        terminal_role: grok_echo.name.clone(),
        max_retries: 0,
    };

    redis_io::save_role(&mut conn, &grok_echo).await?;
    redis_io::save_team(&mut conn, &grok_team).await?;

    tracing::info!(
        "seeded: roles [echo-agent, naughty-agent, speaker-agent, listener-agent, grok-echo] v1, teams [echo-team, naughty-team, speaker-listener-team, grok-echo-team] v1"
    );
    Ok(())
}
