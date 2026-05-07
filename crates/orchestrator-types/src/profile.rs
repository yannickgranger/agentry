//! Profile — per-project configuration manifest.
//!
//! Read from `.agentry/profile.toml` in each target_repo. Declares which
//! tool packs the project's coder and reviewer consume, the canonical
//! brief acceptance command, and which methodology gates apply to
//! dispatched briefs.
//!
//! This module is the SCHEMA only. Fetching the profile from disk or
//! forge is downstream (slice I/2b); composing the profile's tool packs
//! with role.tool_packs at spawn time is downstream (slice I/2c).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level profile container. Aggregates coder, reviewer, acceptance,
/// and methodology sub-sections. Each sub-section is optional; profiles
/// can ship partial coverage.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    pub coder: ProfileRoleSection,
    #[serde(default)]
    pub reviewer: ProfileRoleSection,
    #[serde(default)]
    pub acceptance: ProfileAcceptanceSection,
    #[serde(default)]
    pub methodology: ProfileMethodologySection,
}

/// Per-role config. Used for both `[coder]` and `[reviewer]` sections in
/// `profile.toml`. Currently carries one field: `tool_packs`. Future
/// fields (model override, system prompt prefix, custom permits) will
/// follow the same pattern — opt-in additions, defaulting to no-op.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRoleSection {
    #[serde(default)]
    pub tool_packs: Vec<String>,
}

/// The brief acceptance command default. When a brief is dispatched
/// against this target_repo without an explicit `payload.acceptance`,
/// the daemon fills in the profile's `default`. The brief author can
/// still override per-brief.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileAcceptanceSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Methodology gates to invoke. Maps to skill names like `"discover"`,
/// `"prescribe"`, `"prepare-issue"`, `"verify-issue"`. The methodology
/// runner (slice I/5) will sequence these as pre-conditions before the
/// coder accepts the brief.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileMethodologySection {
    #[serde(default)]
    pub gates: Vec<String>,
}

/// Errors returned by [`parse_profile_toml`].
#[derive(Debug, thiserror::Error)]
pub enum ProfileParseError {
    #[error("invalid profile toml: {0}")]
    Toml(#[from] toml::de::Error),
}

/// Parse a `.agentry/profile.toml` document into a [`Profile`].
///
/// Pure: no I/O. Fetching the source text from disk or forge is the
/// caller's responsibility (slice I/2b).
///
/// Unknown sections or fields are rejected via `deny_unknown_fields`,
/// so operators get an immediate signal that they typoed a section
/// name.
pub fn parse_profile_toml(text: &str) -> Result<Profile, ProfileParseError> {
    let p: Profile = toml::from_str(text)?;
    Ok(p)
}
