//! Review findings — structured output from any role acting as a quality gate.
//!
//! A `ReviewFinding` is the unit the rework loop consumes. The daemon does not
//! interpret `category` or `message`; it only routes findings back to the
//! upstream worker named in the team's `message_graph`. Producers (reviewer
//! roles, coder exitpoints, ci-watcher) emit findings; consumers (coder
//! workers on re-fire) read them out of `TeamContext.messages`.

use serde::{Deserialize, Serialize};

/// How consequential a finding is. Daemon only acts on `Blocker`; `Warn` is
/// informational and does not trigger rework.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Warn,
}

/// Where the finding came from. Downstream tooling (dashboards, chain
/// triggers) can attribute blame without parsing `message`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FindingOrigin {
    /// A deterministic tool produced the finding (cargo fmt, cargo clippy,
    /// cargo test, scripts/arch-check.sh). `tool` names the binary; `rule`
    /// names the specific lint/rule when available.
    Mechanical {
        tool: String,
        #[serde(default)]
        rule: Option<String>,
    },
    /// An LLM-driven reviewer produced the finding.
    Model { reviewer_agent_id: String },
}

/// One actionable issue against a candidate change.
///
/// Round-trips through serde so the daemon can ship it to downstream roles
/// inside a `RoutedMessage.payload`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    pub severity: Severity,
    pub origin: FindingOrigin,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub suggested_fix: Option<String>,
    /// Constraints on the next coder iteration: things it MUST NOT do.
    /// Populated by Blocker findings to anchor rework; empty for Warns.
    #[serde(default)]
    pub prohibitions: Vec<String>,
    /// Constraints on the next coder iteration: things it MUST do.
    /// Populated by Blocker findings to anchor rework; empty for Warns.
    #[serde(default)]
    pub requirements: Vec<String>,
}
