//! AgentRole — container specification for one kind of agent.
//!
//! Lives at `agentry:role:{name}:v{version}`. Typed, edited via dashboard forms.
//! Describes: what model, what tools, what substrate, what binaries, what prompt.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Role name. Lowercase, hyphens only: `coder-rust`, `archaeologist`, `shipper`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoleName(pub String);

impl fmt::Display for RoleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where the agent runs. User picks; orchestrator adapts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubstrateClass {
    /// Rootless podman on the dev box. Default.
    Podman,
    /// Docker daemon.
    Docker,
    /// LXC container.
    Lxc,
    /// Any SSH-reachable Linux box.
    Ssh,
    /// libvirt VM.
    Vm,
}

impl Default for SubstrateClass {
    fn default() -> Self {
        Self::Podman
    }
}

/// What tools the agent is permitted to call. Names are stable symbolic ids;
/// the container runner maps them to actual binaries / MCP methods.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolAllowlist(pub Vec<String>);

impl ToolAllowlist {
    #[must_use]
    pub fn contains(&self, tool: &str) -> bool {
        self.0.iter().any(|t| t == tool)
    }
}

/// An MCP server to mount into the agent's container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// Symbolic name: `ra-query`, `mcp-forge`, etc.
    pub name: String,
    /// Container image to mount (e.g. `ghcr.io/yg/mcp-forge:v0.4.0`).
    pub image: Option<String>,
    /// Or: a local binary path to invoke inside the container.
    pub binary: Option<String>,
}

/// A host→container bind mount, optionally read-only. Used by Claude-Max
/// agents to bring in the `claude` binary and `~/.claude/.credentials.json`
/// without baking them into an image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Absolute host path.
    pub source: String,
    /// Absolute container path.
    pub target: String,
    /// If `true`, the mount is read-only (`:ro`). Defaults to `false`.
    #[serde(default)]
    pub readonly: bool,
}

/// Permission scopes — narrowed further at spawn time by brief/team overrides.
/// Each entry is a symbolic scope string: `fs:read:/workspace/**`, `net:deny:*`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermitScope(pub Vec<String>);

/// An agent role — the full specification for one kind of agent container.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRole {
    pub name: RoleName,
    /// Monotonic version; bump on every save.
    pub version: u32,
    /// Which LLM this role uses (symbolic): `claude-opus-4-7`, `grok-4`, `gemini-2-flash`, etc.
    /// May be `None` for non-LLM roles (scripts, data-producers).
    pub model: Option<String>,
    /// System prompt (inline). Can reference a file as `@file://path` — resolver elsewhere.
    pub system_prompt: Option<String>,
    /// Container image to spawn (fully qualified): `agentry/echo-agent:v1`.
    pub image: String,
    /// Substrate to spawn on.
    #[serde(default)]
    pub substrate_class: SubstrateClass,
    /// Extra binaries to install in the container at spawn.
    #[serde(default)]
    pub binaries: Vec<String>,
    /// MCP servers to mount.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    /// Whitelist of tool names the agent may call.
    #[serde(default)]
    pub tool_allowlist: ToolAllowlist,
    /// Base permit scope — narrowed further per brief.
    #[serde(default)]
    pub permit_scope: PermitScope,
    /// Environment-variable names to pass through from orchestratord's env
    /// to the spawned container (e.g. `["XAI_API_KEY"]`). Values are NEVER
    /// stored in the role; the orchestrator reads them from its own env at
    /// spawn time. Missing vars are silently skipped (broker logs a warning).
    #[serde(default)]
    pub passthru_env: Vec<String>,
    /// Host→container bind mounts. Used by Claude-Max agents to bring in the
    /// `claude` binary and credentials file without baking them into an image.
    #[serde(default)]
    pub mounts: Vec<Mount>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_roundtrip_json() {
        let r = AgentRole {
            name: RoleName("coder-rust".into()),
            version: 3,
            model: Some("claude-opus-4-7".into()),
            system_prompt: Some("You are a Rust coder. Follow the contract.".into()),
            image: "agentry/coder-rust:v3".into(),
            substrate_class: SubstrateClass::Podman,
            binaries: vec!["cargo".into(), "rustfmt".into()],
            mcp_servers: vec![McpServer {
                name: "ra-query".into(),
                image: Some("ghcr.io/yg/ra-query:v0.1".into()),
                binary: None,
            }],
            tool_allowlist: ToolAllowlist(vec!["read".into(), "edit".into(), "bash:cargo".into()]),
            permit_scope: PermitScope(vec![
                "fs:read:/workspace/**".into(),
                "fs:write:/workspace/**".into(),
                "net:deny:*".into(),
            ]),
            passthru_env: vec![],
            mounts: vec![],
        };
        let s = serde_json::to_string_pretty(&r).expect("ser");
        let back: AgentRole = serde_json::from_str(&s).expect("de");
        assert_eq!(r, back);
    }

    #[test]
    fn default_substrate_is_podman() {
        assert_eq!(SubstrateClass::default(), SubstrateClass::Podman);
    }

    #[test]
    fn allowlist_contains_works() {
        let a = ToolAllowlist(vec!["read".into(), "edit".into()]);
        assert!(a.contains("read"));
        assert!(!a.contains("write"));
    }
}
