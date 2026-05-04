//! WorkPermit — the signed permission for one agent to perform one brief's role.
//!
//! Based on the AEGIS pattern (salvaged from v1 `agency-aegis`). Signing is
//! added in M3; M0 ships an unsigned structural draft so the runtime shape
//! is fixed early.

pub use crate::role::{AllowedTools, PermitScope, ToolAllowlist};
use crate::{brief::BriefId, role::RoleName, Ts};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkPermit {
    /// Permit id; unique per mint.
    pub permit_id: String,
    /// Agent session id this permit binds to.
    pub agent_id: String,
    /// Role the permit covers.
    pub role: RoleName,
    /// Brief the agent is working on.
    pub brief: BriefId,
    /// Which tools the agent may call.
    pub tool_allowlist: ToolAllowlist,
    /// Pre-spawn `claude --allowedTools` patterns, copied through from the
    /// role at mint time. Distinct from `tool_allowlist` and intentionally
    /// NOT auto-synchronized — see [`AllowedTools`] docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<AllowedTools>,
    /// Capability scopes (fs, net, …).
    pub permit_scope: PermitScope,
    /// Budget — duplicated from the brief, locked at mint time.
    pub max_tokens: Option<u64>,
    pub max_wall_seconds: Option<u64>,
    pub max_usd: Option<f64>,
    /// When the permit expires (hard wall).
    pub expires_at: Ts,
    /// When the permit was minted.
    pub issued_at: Ts,
    /// ed25519 signature — populated in M3.
    #[serde(default)]
    pub signature: Option<String>,
}

impl WorkPermit {
    /// Check if a tool is in the allowlist.
    #[must_use]
    pub fn allows(&self, tool: &str) -> bool {
        self.tool_allowlist.contains(tool)
    }
}
