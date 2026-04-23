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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubstrateClass {
    /// Rootless podman on the dev box. Default.
    #[default]
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

/// Which package manager the spawner uses to install `binaries` at spawn time.
/// Picked explicitly per role; no heuristic from image name. `None` disables
/// install entirely (used when the base image already contains everything
/// the entrypoint needs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    /// No install at spawn — script runs directly. Default.
    #[default]
    None,
    /// Alpine — `apk add --no-cache <binaries>`.
    Apk,
    /// Debian/Ubuntu — `apt-get update && apt-get install -y <binaries>`.
    Apt,
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
    /// Container image to spawn. Either a stock public image
    /// (`alpine:3.21`, `debian:bookworm-slim`) with an `entrypoint_script`
    /// provisioned at spawn, or a pre-built image embedding its own entrypoint
    /// (legacy path — left supported for roles that genuinely need baking).
    pub image: String,
    /// Substrate to spawn on.
    #[serde(default)]
    pub substrate_class: SubstrateClass,
    /// Package manager to use when installing `binaries` at spawn. `None`
    /// skips install (base image has everything).
    #[serde(default)]
    pub package_manager: PackageManager,
    /// Inline entrypoint script (bash). When set, spawner delivers it to the
    /// container via the `AGENTRY_SCRIPT` env var, installs `binaries` via
    /// `package_manager`, then execs it. When `None`, spawner falls back to
    /// the image's baked ENTRYPOINT (legacy path).
    #[serde(default)]
    pub entrypoint_script: Option<String>,
    /// Package names to install at spawn via `package_manager`. The spawner
    /// always adds a baseline (`bash ca-certificates coreutils jq`); this
    /// list is role-specific extras (e.g. `["git", "curl"]`).
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
            image: "alpine:3.21".into(),
            substrate_class: SubstrateClass::Podman,
            package_manager: PackageManager::Apk,
            entrypoint_script: Some("#!/usr/bin/env bash\necho hello\n".into()),
            binaries: vec!["git".into(), "curl".into()],
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
    fn default_package_manager_is_none() {
        assert_eq!(PackageManager::default(), PackageManager::None);
    }

    #[test]
    fn role_without_entrypoint_script_deserializes() {
        // Backward-compat: old role JSON without entrypoint_script / package_manager
        // fields must still deserialize cleanly, defaulting to None / PackageManager::None.
        let json = r#"{
            "name": "legacy-agent",
            "version": 1,
            "model": null,
            "system_prompt": null,
            "image": "agentry/legacy-agent:v1"
        }"#;
        let r: AgentRole = serde_json::from_str(json).expect("de");
        assert!(r.entrypoint_script.is_none());
        assert_eq!(r.package_manager, PackageManager::None);
        assert!(r.binaries.is_empty());
    }

    #[test]
    fn allowlist_contains_works() {
        let a = ToolAllowlist(vec!["read".into(), "edit".into()]);
        assert!(a.contains("read"));
        assert!(!a.contains("write"));
    }
}
