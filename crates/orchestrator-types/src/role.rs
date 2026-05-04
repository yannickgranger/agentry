//! AgentRole — container specification for one kind of agent.
//!
//! Lives at `agentry:role:{name}:v{version}`. Typed, edited via dashboard forms.
//! Describes: what model, what tools, what substrate, what binaries, what prompt.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Role name. Lowercase, hyphens only: `coder-rust`, `archaeologist`, `shipper`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RoleName(pub String);

impl fmt::Display for RoleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A version-pinned reference to a role: `(name, version)`. The team topology
/// pins each role to a specific version so that a topology committed today
/// keeps resolving to the exact role specs it was authored against — even as
/// new role versions are registered.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleRef {
    pub name: RoleName,
    pub version: u32,
}

/// Where the agent runs. User picks; orchestrator adapts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
/// Picked explicitly per role; no heuristic from image name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    /// Alpine — `apk add --no-cache <binaries>`.
    Apk,
    /// Debian/Ubuntu — `apt-get update && apt-get install -y <binaries>`.
    Apt,
}

/// What tools the agent is permitted to call. Names are stable symbolic ids;
/// the container runner maps them to actual binaries / MCP methods.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ToolAllowlist(pub Vec<String>);

impl ToolAllowlist {
    #[must_use]
    pub fn contains(&self, tool: &str) -> bool {
        self.0.iter().any(|t| t == tool)
    }
}

/// Pattern strings handed to `claude --allowedTools` at agent spawn time. The
/// grammar is open-ended (`Bash(cargo fmt:*)`, `Read`, `Edit(*.rs)`) and
/// matches what the Claude CLI accepts directly — no symbolic translation.
///
/// Distinct value domain from [`ToolAllowlist`]:
/// - [`AllowedTools`] fences the Claude process *pre-spawn* by being passed
///   through to `claude --allowedTools`, so violations never reach the
///   daemon at all.
/// - [`ToolAllowlist`] carries exact-match symbolic names (`bash`, `read`,
///   `edit`) that the daemon's permit broker checks *post-hoc* against
///   `EventKind::ToolCall` events (see `permits/src/lib.rs`).
///
/// The two are intentionally NOT auto-synchronized — they enforce at
/// different layers with different grammars.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct AllowedTools(pub Vec<String>);

/// An MCP server to mount into the agent's container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Absolute host path.
    pub source: String,
    /// Absolute container path.
    pub target: String,
    /// If `true`, the mount is read-only (`:ro`). Defaults to `false`.
    #[serde(default)]
    pub readonly: bool,
}

/// Declaration that a role wants the brief's workspace bind-mounted into its
/// container. The host path is allocated by the daemon at brief dispatch; the
/// role only names the container-side mount point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMount {
    /// Absolute container path where the brief's workspace appears, e.g. `/workspace`.
    pub container_path: String,
    /// If `true`, the mount is read-only (`:ro`). Defaults to `false`.
    #[serde(default)]
    pub readonly: bool,
}

/// Permission scopes — narrowed further at spawn time by brief/team overrides.
/// Each entry is a symbolic scope string: `fs:read:/workspace/**`, `net:deny:*`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PermitScope(pub Vec<String>);

/// An agent role — the full specification for one kind of agent container.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Package manager to use when installing `binaries` at spawn.
    pub package_manager: PackageManager,
    /// Inline entrypoint script (bash). The spawner delivers it to the
    /// container via the `AGENTRY_SCRIPT` env var, installs `binaries` via
    /// `package_manager`, then execs it. Required — every role ships its
    /// own script.
    pub entrypoint_script: String,
    /// Optional post-worker script. When set, the spawner exports it as
    /// `AGENTRY_EXITPOINT` and the container's bootstrap execs it ONLY if
    /// the entrypoint returned 0. Used for role-local gates (e.g.
    /// `quality-hygiene --fix`) that run AFTER the worker (claude -p,
    /// cargo test) and BEFORE the terminal verdict event. Findings emitted
    /// from the exitpoint accumulate into the role's Verdict. `None` means
    /// the entrypoint is solely responsible for emitting `done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exitpoint_script: Option<String>,
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
    /// Pattern strings handed to `claude --allowedTools` at agent spawn time.
    /// Distinct value domain from `tool_allowlist` and intentionally NOT
    /// auto-synchronized — see [`AllowedTools`] docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<AllowedTools>,
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
    /// Whether this role needs the brief's per-brief workspace mounted.
    /// When `Some`, the daemon allocates a host dir per brief and bind-mounts
    /// it at the declared container path. `None` means the role runs without
    /// a brief workspace (echo/naughty/speaker/listener etc.).
    #[serde(default)]
    pub workspace_mount: Option<WorkspaceMount>,
    /// Wire the container to the agentry-scoped sccache-redis cache. The
    /// spawner auto-installs `sccache` via `package_manager`, sets
    /// `RUSTC_WRAPPER`, and points `SCCACHE_REDIS_ENDPOINT` at
    /// `redis://agentry-sccache-redis:6379` on the `agentry-net` podman
    /// network. Roles that never compile Rust leave this `false` (default).
    #[serde(default)]
    pub sccache: bool,
    /// Extra shell commands executed as part of the container's bootstrap
    /// sequence, one per entry, appended AFTER the package-manager install
    /// and BEFORE the role's entrypoint script. Typical use:
    /// `rustup component add rustfmt clippy` for rust-based roles. Empty =
    /// no extras.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_bootstrap: Vec<String>,
}
