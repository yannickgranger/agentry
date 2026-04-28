//! No-op stub Validator implementations.
//!
//! Every `run` impl returns `ValidatorReport::pass(self.name())`. Brief 3
//! ports the existing reviewer-mechanical logic into real implementations.

use async_trait::async_trait;

use crate::{BriefCtx, Validator, ValidatorReport};

macro_rules! stub_validator {
    ($ty:ident, $static_name:ident, $name:literal) => {
        pub struct $ty;
        pub static $static_name: $ty = $ty;

        #[async_trait]
        impl Validator for $ty {
            fn name(&self) -> &'static str {
                $name
            }
            async fn run(&self, _ctx: &BriefCtx) -> anyhow::Result<ValidatorReport> {
                Ok(ValidatorReport::pass(self.name()))
            }
        }
    };
}

stub_validator!(FmtCheck, FMT_CHECK, "fmt_check");
stub_validator!(ClippyScoped, CLIPPY_SCOPED, "clippy_scoped");
stub_validator!(ClippyWorkspace, CLIPPY_WORKSPACE, "clippy_workspace");
stub_validator!(TestWorkspace, TEST_WORKSPACE, "test_workspace");
stub_validator!(ArchCheck, ARCH_CHECK, "arch_check");
stub_validator!(DeadPubCheck, DEAD_PUB_CHECK, "dead_pub_check");
stub_validator!(
    ComplexityNoRegression,
    COMPLEXITY_NO_REGRESSION,
    "complexity_no_regression"
);
stub_validator!(NoNewPub, NO_NEW_PUB, "no_new_pub");
stub_validator!(RegressionTest, REGRESSION_TEST, "regression_test");
stub_validator!(MarkdownLint, MARKDOWN_LINT, "markdown_lint");
stub_validator!(BddRealInfra, BDD_REAL_INFRA, "bdd_real_infra");
stub_validator!(SelfHostSmoke, SELF_HOST_SMOKE, "self_host_smoke");
stub_validator!(ReportOnly, REPORT_ONLY, "report_only");
stub_validator!(NoBehaviorChange, NO_BEHAVIOR_CHANGE, "no_behavior_change");
stub_validator!(SpecsArchCheck, SPECS_ARCH_CHECK, "specs_arch_check");
