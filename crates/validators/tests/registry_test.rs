use orchestrator_types::BriefKind;
use std::collections::HashSet;
use std::path::PathBuf;
use validators::{stubs, BriefCtx, Finding, Severity, Validator, ValidatorReport};

fn names(kind: BriefKind) -> Vec<&'static str> {
    validators::registry_for(kind)
        .iter()
        .map(|v| v.name())
        .collect()
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
            !validators::registry_for(k).is_empty(),
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

#[test]
fn with_message_pushes_blocker_finding() {
    let r = ValidatorReport::fail("dispatch", vec![]).with_message("boom".to_string());
    assert!(!r.passed);
    assert_eq!(r.findings.len(), 1);
    assert_eq!(r.findings[0].severity, Severity::Blocker);
    assert_eq!(r.findings[0].message, "boom");
}

#[test]
fn report_serializes_to_json_with_expected_fields() {
    let r = ValidatorReport::fail(
        "x",
        vec![Finding {
            file: Some("a.rs".into()),
            line: Some(3),
            severity: Severity::Blocker,
            message: "m".into(),
        }],
    );
    let v = serde_json::to_value(&r).expect("serialize");
    assert_eq!(v["validator_name"], "x");
    assert_eq!(v["passed"], false);
    assert_eq!(v["findings"][0]["severity"], "blocker");
    assert_eq!(v["findings"][0]["message"], "m");
}
