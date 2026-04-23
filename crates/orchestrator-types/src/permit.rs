//! WorkPermit — the signed permission for one agent to perform one brief's role.
//!
//! Based on the AEGIS pattern (salvaged from v1 `agency-aegis`). Signing is
//! added in M3; M0 ships an unsigned structural draft so the runtime shape
//! is fixed early.

pub use crate::role::{PermitScope, ToolAllowlist};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::now;

    #[test]
    fn permit_roundtrip_json() {
        let p = WorkPermit {
            permit_id: "prm_test".into(),
            agent_id: "agt_1".into(),
            role: RoleName("coder-rust".into()),
            brief: BriefId("brf_1".into()),
            tool_allowlist: ToolAllowlist(vec!["read".into(), "edit".into()]),
            permit_scope: PermitScope(vec!["fs:read:/workspace/**".into()]),
            max_tokens: Some(500_000),
            max_wall_seconds: Some(3600),
            max_usd: Some(5.0),
            expires_at: now() + chrono::Duration::hours(1),
            issued_at: now(),
            signature: None,
        };
        let s = serde_json::to_string(&p).expect("ser");
        let back: WorkPermit = serde_json::from_str(&s).expect("de");
        assert_eq!(p, back);
    }

    #[test]
    fn permit_allows_checks_allowlist() {
        let p = WorkPermit {
            permit_id: "x".into(),
            agent_id: "x".into(),
            role: RoleName("x".into()),
            brief: BriefId("x".into()),
            tool_allowlist: ToolAllowlist(vec!["read".into()]),
            permit_scope: PermitScope::default(),
            max_tokens: None,
            max_wall_seconds: None,
            max_usd: None,
            expires_at: now(),
            issued_at: now(),
            signature: None,
        };
        assert!(p.allows("read"));
        assert!(!p.allows("write"));
    }
}
