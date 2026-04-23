//! Seed the Redis registry with the echo-team and echo-agent role.
//!
//! Idempotent: overwrites existing records with current definitions.

use crate::{Config, Result, redis_io};
use orchestrator_types::{
    AgentRole, MessageEdge, Mount, PermitScope, RoleName, SubstrateClass, TeamName, TeamTopology,
    ToolAllowlist,
};

/// Seed the M0/M3 registry using the URL from `Config`.
pub async fn seed_m0(cfg: &Config) -> Result<()> {
    let mut conn = redis_io::connect(&cfg.redis.url).await?;

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
        mounts: vec![],
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
        mounts: vec![],
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
        mounts: vec![],
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
        mounts: vec![],
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
        mounts: vec![],
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

    // M5b: claude-echo — Claude Max via host `claude` CLI.
    // Paths are computed from HOME at seed-time so different machines can reuse the same seed binary.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/home/yg".into());
    let claude_echo = AgentRole {
        name: RoleName("claude-echo".into()),
        version: 1,
        model: Some("claude-max".into()),
        system_prompt: None,
        image: "localhost/agentry/claude-agent:v1".into(),
        substrate_class: SubstrateClass::Podman,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec![
            "net:allow:api.anthropic.com".into(), // claude CLI does call this, authed via OAuth — NOT via API key
        ]),
        passthru_env: vec![],
        mounts: vec![
            Mount {
                source: format!("{home}/.local/bin/claude"),
                target: "/usr/local/bin/claude".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.claude/.credentials.json"),
                target: "/root/.claude/.credentials.json".into(),
                readonly: true,
            },
            Mount {
                source: format!("{home}/.claude/settings.json"),
                target: "/root/.claude/settings.json".into(),
                readonly: true,
            },
        ],
    };
    let claude_team = TeamTopology {
        name: TeamName("claude-echo-team".into()),
        version: 1,
        roles: vec![claude_echo.name.clone()],
        message_graph: vec![],
        terminal_role: claude_echo.name.clone(),
        max_retries: 0,
    };

    redis_io::save_role(&mut conn, &claude_echo).await?;
    redis_io::save_team(&mut conn, &claude_team).await?;

    // M6: synthesizer + narrowed-coder. Synthesizer emits a Message with
    // `permit_overrides.fs_write`, which the daemon applies to the coder's
    // permit before spawn. Coder tries to write outside the scope → blocked.
    let synthesizer = AgentRole {
        name: RoleName("synthesizer".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "localhost/agentry/synthesizer-agent:v1".into(),
        substrate_class: SubstrateClass::Podman,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist(vec![]),
        permit_scope: PermitScope(vec!["net:deny:*".into()]),
        passthru_env: vec![],
        mounts: vec![],
    };
    let narrowed_coder = AgentRole {
        name: RoleName("narrowed-coder".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "localhost/agentry/narrowed-coder-agent:v1".into(),
        substrate_class: SubstrateClass::Podman,
        binaries: vec![],
        mcp_servers: vec![],
        // The broad base — will be narrowed by synthesizer's Message.
        tool_allowlist: ToolAllowlist(vec!["write".into(), "edit".into(), "read".into()]),
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:deny:*".into(),
        ]),
        passthru_env: vec![],
        mounts: vec![],
    };
    let narrowed_team = TeamTopology {
        name: TeamName("narrowed-team".into()),
        version: 1,
        roles: vec![synthesizer.name.clone(), narrowed_coder.name.clone()],
        message_graph: vec![orchestrator_types::MessageEdge {
            from: synthesizer.name.clone(),
            to: narrowed_coder.name.clone(),
            permit_overrides_from: Some("permit_overrides".into()),
        }],
        terminal_role: narrowed_coder.name.clone(),
        max_retries: 0,
    };

    redis_io::save_role(&mut conn, &synthesizer).await?;
    redis_io::save_role(&mut conn, &narrowed_coder).await?;
    redis_io::save_team(&mut conn, &narrowed_team).await?;

    tracing::info!(
        "seeded: roles [echo, naughty, speaker, listener, grok-echo, claude-echo, synthesizer, narrowed-coder] v1; teams [echo, naughty, speaker-listener, grok-echo, claude-echo, narrowed-team] v1"
    );
    Ok(())
}
