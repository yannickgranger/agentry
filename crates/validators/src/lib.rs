//! Validator pipeline for briefs.
//!
//! Brief 2 of EPIC #152 — type-level scaffolding only. All `Validator` impls
//! in [`stubs`] return [`ValidatorReport::pass`]; brief 3 ports the existing
//! reviewer-mechanical logic into real implementations. Nothing reads
//! `brief.kind` yet — wiring lands in brief 4.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use orchestrator_types::BriefKind;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Blocker,
    Warning,
    Info,
}

/// A single observation produced by a validator.
#[derive(Debug, Clone)]
pub struct Finding {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub severity: Severity,
    pub message: String,
}

/// Outcome of a [`Validator`] run.
#[derive(Debug, Clone)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn names(kind: BriefKind) -> Vec<&'static str> {
        registry_for(kind).iter().map(|v| v.name()).collect()
    }

    #[test]
    fn test_mechanical_dispatch() {
        assert_eq!(
            names(BriefKind::Mechanical),
            vec![
                "fmt_check",
                "clippy_scoped",
                "no_behavior_change",
                "arch_check"
            ]
        );
    }

    #[test]
    fn test_refactor_dispatch() {
        assert_eq!(
            names(BriefKind::Refactor),
            vec![
                "clippy_workspace",
                "test_workspace",
                "arch_check",
                "complexity_no_regression",
                "no_new_pub",
            ]
        );
    }

    #[test]
    fn test_debug_dispatch() {
        assert_eq!(
            names(BriefKind::Debug),
            vec!["regression_test", "test_workspace", "arch_check"]
        );
    }

    #[test]
    fn test_new_feature_dispatch() {
        assert_eq!(
            names(BriefKind::NewFeature),
            vec![
                "bdd_real_infra",
                "test_workspace",
                "arch_check",
                "clippy_workspace",
            ]
        );
    }

    #[test]
    fn test_substrate_dispatch() {
        assert_eq!(
            names(BriefKind::Substrate),
            vec![
                "self_host_smoke",
                "test_workspace",
                "arch_check",
                "clippy_workspace",
            ]
        );
    }

    #[test]
    fn test_audit_dispatch() {
        assert_eq!(names(BriefKind::Audit), vec!["report_only"]);
    }

    #[test]
    fn test_doc_dispatch() {
        assert_eq!(
            names(BriefKind::Doc),
            vec!["markdown_lint", "specs_arch_check"]
        );
    }

    fn all_kinds() -> Vec<BriefKind> {
        vec![
            BriefKind::Refactor,
            BriefKind::Debug,
            BriefKind::Mechanical,
            BriefKind::NewFeature,
            BriefKind::Substrate,
            BriefKind::Audit,
            BriefKind::Doc,
        ]
    }

    #[test]
    fn test_all_kinds_have_at_least_one_validator() {
        for k in all_kinds() {
            assert!(
                !registry_for(k).is_empty(),
                "kind {k:?} has empty validator pipeline"
            );
        }
    }

    #[test]
    fn test_validator_names_are_unique_per_kind() {
        for k in all_kinds() {
            let ns = names(k);
            let set: HashSet<&&str> = ns.iter().collect();
            assert_eq!(
                set.len(),
                ns.len(),
                "duplicate validator name in pipeline for {k:?}: {ns:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_stub_validators_pass() {
        let v: &'static dyn Validator = &stubs::REPORT_ONLY;
        let ctx = BriefCtx {
            workspace_path: PathBuf::from("/tmp/does-not-matter"),
            brief_id: "brf_test".into(),
            changed_files: vec![],
        };
        let report = v.run(&ctx).await.expect("stub run is infallible");
        assert!(report.passed);
        assert!(report.findings.is_empty());
        assert_eq!(report.validator_name, "report_only");
    }
}
