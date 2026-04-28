//! Project — scoping record for a body of work (e.g. `qbot-core`, `trading-research`).
//!
//! A project is *not* a new primitive at runtime. It's a tiny record that lets
//! the orchestrator scope budget, pick a default topology, and namespace memory.
//! Per the design: ~40 lines of actual information.

use crate::brief::EscalationMode;
use crate::team::TeamName;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectSlug(pub String);

impl fmt::Display for ProjectSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Standing orders: constraints that apply to every brief in this project.
/// Agents read these from their brief context; the orchestrator enforces budget.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StandingOrders {
    /// Daily token cap across all briefs in this project.
    pub tokens_daily: Option<u64>,
    /// Daily USD cap.
    pub usd_daily: Option<f64>,
    /// Default escalation mode when a brief doesn't specify.
    #[serde(default)]
    pub default_escalation: EscalationMode,
    /// Freeform priorities passed to the steward team.
    #[serde(default)]
    pub priorities: Vec<String>,
    /// Forbidden operations (symbolic): `git:force-push:main`, `delete:branch:*`, etc.
    #[serde(default)]
    pub forbidden: Vec<String>,
}

/// A project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub slug: ProjectSlug,
    pub name: String,
    /// Git forges this project lives on: `agency:yg/qbot-core`, `github:yg/other-repo`.
    pub forges: Vec<String>,
    /// Default topology used if a brief doesn't specify.
    pub default_topology: Option<TeamName>,
    /// Topology used for steward runs.
    pub steward_topology: Option<TeamName>,
    #[serde(default)]
    pub standing_orders: StandingOrders,
    /// Optional forge URL for the project's primary repo. When set, briefs
    /// naming this project get their workspace allocated as a `git worktree`
    /// off a shared bare clone at `<workspace-root>/.clones/<org>/<repo>/`.
    /// Example: `https://oauth2:${GITEA_TOKEN}@agency.lab:3000/yg/agentry.git`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    /// Optional base branch. When set together with `repo_url`, this is the
    /// ref the bare clone tracks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_briefs: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_roundtrip_json() {
        let p = Project {
            slug: ProjectSlug("qbot-core".into()),
            name: "qbot-core".into(),
            forges: vec!["agency:yg/qbot-core".into()],
            default_topology: Some(TeamName("qbot-issue-team".into())),
            steward_topology: Some(TeamName("qbot-steward".into())),
            standing_orders: StandingOrders {
                tokens_daily: Some(2_000_000),
                usd_daily: Some(20.0),
                default_escalation: EscalationMode::Autonomous,
                priorities: vec!["close RFC-023".into()],
                forbidden: vec!["git:force-push:main".into()],
            },
            repo_url: None,
            base_branch: None,
            max_concurrent_briefs: None,
        };
        let s = serde_json::to_string(&p).expect("ser");
        let back: Project = serde_json::from_str(&s).expect("de");
        assert_eq!(p, back);
    }

    #[test]
    fn project_roundtrips_with_repo_url() {
        let p = Project {
            slug: ProjectSlug("agentry".into()),
            name: "agentry".into(),
            forges: vec!["agency:yg/agentry".into()],
            default_topology: Some(TeamName("agentry-self-host-v0".into())),
            steward_topology: None,
            standing_orders: StandingOrders::default(),
            repo_url: Some("https://agency.lab:3000/yg/agentry.git".into()),
            base_branch: Some("develop".into()),
            max_concurrent_briefs: None,
        };
        let s = serde_json::to_string(&p).expect("ser");
        let back: Project = serde_json::from_str(&s).expect("de");
        assert_eq!(p, back);
    }

    #[test]
    fn project_roundtrips_with_max_concurrent_briefs() {
        let p = Project {
            slug: ProjectSlug("agentry".into()),
            name: "agentry".into(),
            forges: vec!["agency:yg/agentry".into()],
            default_topology: None,
            steward_topology: None,
            standing_orders: StandingOrders::default(),
            repo_url: None,
            base_branch: None,
            max_concurrent_briefs: Some(2),
        };
        let s = serde_json::to_string(&p).expect("ser");
        let back: Project = serde_json::from_str(&s).expect("de");
        assert_eq!(p, back);
    }
}
