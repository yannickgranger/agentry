//! Validator pipeline for briefs.
//!
//! Brief 2 of EPIC #152 — type-level scaffolding only. All `Validator` impls
//! in [`stubs`] return [`ValidatorReport::pass`]; brief 3 ports the existing
//! reviewer-mechanical logic into real implementations. Nothing reads
//! `brief.kind` yet — wiring lands in brief 4.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use orchestrator_types::BriefKind;
use serde::Serialize;
use std::path::PathBuf;

pub mod impls;
pub mod stubs;

/// Per-brief context handed to every [`Validator`].
#[derive(Debug, Clone)]
pub struct BriefCtx {
    /// Workspace path the validator should inspect.
    pub workspace_path: PathBuf,
    /// Brief identifier (for logs / report attribution).
    pub brief_id: String,
    /// Files changed by the coder vs `base_branch` — populated by the ship
    /// tool from `git diff --name-only`. Empty in stubs/tests.
    pub changed_files: Vec<PathBuf>,
}

/// Severity of a [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Warning,
    Info,
}

/// A single observation produced by a validator.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub severity: Severity,
    pub message: String,
}

/// Outcome of a [`Validator`] run.
#[derive(Debug, Clone, Serialize)]
pub struct ValidatorReport {
    pub validator_name: String,
    pub passed: bool,
    pub findings: Vec<Finding>,
}

impl ValidatorReport {
    #[must_use]
    pub fn pass(name: &str) -> Self {
        Self {
            validator_name: name.into(),
            passed: true,
            findings: vec![],
        }
    }

    #[must_use]
    pub fn fail(name: &str, findings: Vec<Finding>) -> Self {
        Self {
            validator_name: name.into(),
            passed: false,
            findings,
        }
    }

    /// Append a Blocker [`Finding`] carrying `msg` and return self. Used by
    /// the ship binary to surface dispatch / panic errors uniformly with
    /// validator-reported failures.
    #[must_use]
    pub fn with_message(mut self, msg: String) -> Self {
        self.findings.push(Finding {
            file: None,
            line: None,
            severity: Severity::Blocker,
            message: msg,
        });
        self
    }
}

#[async_trait]
pub trait Validator: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, ctx: &BriefCtx) -> anyhow::Result<ValidatorReport>;
}

/// Resolve the validator pipeline for a given brief kind.
///
/// Order is the order validators run in (sequential; the ship tool may choose
/// to run them in parallel via `JoinSet` — order here is informational).
#[must_use]
pub fn registry_for(kind: BriefKind) -> Vec<&'static dyn Validator> {
    use impls::{ARCH_CHECK, CLIPPY_SCOPED, CLIPPY_WORKSPACE, FMT_CHECK, TEST_WORKSPACE};
    use stubs::{
        BDD_REAL_INFRA, COMPLEXITY_NO_REGRESSION, MARKDOWN_LINT, NO_BEHAVIOR_CHANGE, NO_NEW_PUB,
        REGRESSION_TEST, REPORT_ONLY, SELF_HOST_SMOKE, SPECS_ARCH_CHECK,
    };
    match kind {
        BriefKind::Mechanical => vec![&FMT_CHECK, &CLIPPY_SCOPED, &NO_BEHAVIOR_CHANGE, &ARCH_CHECK],
        BriefKind::Refactor => vec![
            &CLIPPY_WORKSPACE,
            &TEST_WORKSPACE,
            &ARCH_CHECK,
            &COMPLEXITY_NO_REGRESSION,
            &NO_NEW_PUB,
        ],
        BriefKind::Debug => vec![&REGRESSION_TEST, &TEST_WORKSPACE, &ARCH_CHECK],
        BriefKind::NewFeature => vec![
            &BDD_REAL_INFRA,
            &TEST_WORKSPACE,
            &ARCH_CHECK,
            &CLIPPY_WORKSPACE,
        ],
        BriefKind::Substrate => vec![
            &SELF_HOST_SMOKE,
            &TEST_WORKSPACE,
            &ARCH_CHECK,
            &CLIPPY_WORKSPACE,
        ],
        BriefKind::Audit => vec![&REPORT_ONLY],
        BriefKind::Doc => vec![&MARKDOWN_LINT, &SPECS_ARCH_CHECK],
    }
}
