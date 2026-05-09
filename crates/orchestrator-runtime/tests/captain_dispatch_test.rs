//! Integration tests for `captain dispatch`.
//!
//! Tests run the compiled `captain` binary as a subprocess and write
//! temporary brief files via `tempfile`. Tests use `--dry-run` so the live
//! `orchestrator submit` path is not exercised here — that path is exercised
//! at acceptance time when this brief itself is dispatched.

use orchestrator_types::{
    now, Assertion, AssertionAnchor, AssertionId, Brief, BriefId, Budget, Contract, EscalationMode,
    TaskShape, VersionedRef,
};
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use tempfile::NamedTempFile;

fn captain_bin() -> &'static str {
    env!("CARGO_BIN_EXE_captain")
}

fn write_temp_brief(json: Value) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("create tempfile");
    let text = serde_json::to_string_pretty(&json).expect("serialize brief json");
    f.write_all(text.as_bytes()).expect("write brief json");
    f.flush().expect("flush brief json");
    f
}

fn build_brief(kind: Option<TaskShape>, contract: Option<Contract>) -> Brief {
    Brief {
        id: BriefId("brf_test_dispatch".into()),
        project: None,
        topology: VersionedRef::new("agentry-bugfix-v0", 1),
        payload: serde_json::json!({
            "issue_title": "test",
            "issue_body": "test",
            "acceptance": "true",
            "target_repo": "yg/agentry",
            "base_branch": "develop",
            "pr_title": "test",
            "pr_body": "test",
        }),
        kind,
        contract,
        budget: Budget::default(),
        escalation: EscalationMode::Autonomous,
        parent_brief: None,
        cohort_labels: Vec::new(),
        redeploy_required: vec![],
        submitted_by: "captain-cli-test".to_string(),
        submitted_at: now(),
    }
}

fn run_dispatch(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(captain_bin());
    cmd.arg("dispatch");
    cmd.args(args);
    cmd.output().expect("spawn captain dispatch")
}

#[test]
fn captain_dispatch_dry_run_validates_minimal_trivial_doc() {
    let brief = build_brief(Some(TaskShape::TrivialDoc), None);
    let json = serde_json::to_value(&brief).expect("brief to value");
    let f = write_temp_brief(json);
    let path = f.path().to_str().expect("tempfile path utf-8");
    let out = run_dispatch(&["--dry-run", path]);
    assert!(
        out.status.success(),
        "captain dispatch --dry-run should succeed; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("validated"),
        "expected `validated` in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&brief.id.0),
        "expected brief id `{}` in stderr:\n{stderr}",
        brief.id.0
    );
    assert!(
        stderr.contains("kind=Some(TrivialDoc)"),
        "expected `kind=Some(TrivialDoc)` in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("contract_present=false"),
        "expected `contract_present=false` in stderr:\n{stderr}"
    );
}

#[test]
fn captain_dispatch_dry_run_succeeds_on_feature_with_contract() {
    let brief_id = BriefId("brf_test_feature_contract".into());
    let contract = Contract {
        brief_id: brief_id.clone(),
        assertions: vec![Assertion {
            id: AssertionId("A1".into()),
            prose: "feature must compile".into(),
            anchor: AssertionAnchor::Cfdb {
                qname: "crate::feature::foo".into(),
            },
        }],
        precursor_artifacts: Vec::new(),
    };
    let mut brief = build_brief(Some(TaskShape::Feature), Some(contract));
    brief.id = brief_id;
    let json = serde_json::to_value(&brief).expect("brief to value");
    let f = write_temp_brief(json);
    let path = f.path().to_str().expect("tempfile path utf-8");
    let out = run_dispatch(&["--dry-run", path]);
    assert!(
        out.status.success(),
        "captain dispatch --dry-run should succeed; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("contract_present=true"),
        "expected `contract_present=true` in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("assertions=1"),
        "expected `assertions=1` in stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("WARN"),
        "expected no WARN line for feature-with-contract:\n{stderr}"
    );
}

#[test]
fn captain_dispatch_dry_run_warns_on_feature_without_contract() {
    let brief = build_brief(Some(TaskShape::Feature), None);
    let json = serde_json::to_value(&brief).expect("brief to value");
    let f = write_temp_brief(json);
    let path = f.path().to_str().expect("tempfile path utf-8");
    let out = run_dispatch(&["--dry-run", path]);
    assert!(
        out.status.success(),
        "captain dispatch --dry-run should succeed even with WARN; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WARN"),
        "expected `WARN` in stderr for feature-without-contract:\n{stderr}"
    );
    assert!(
        stderr.contains("requires contract"),
        "expected `requires contract` in stderr:\n{stderr}"
    );
}

#[test]
fn captain_dispatch_rejects_invalid_brief_json() {
    let mut f = NamedTempFile::new().expect("create tempfile");
    f.write_all(b"{ this is not valid json")
        .expect("write malformed json");
    f.flush().expect("flush malformed json");
    let path = f.path().to_str().expect("tempfile path utf-8");
    let out = run_dispatch(&["--dry-run", path]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for malformed brief; status={:?} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parse") || stderr.contains("Brief"),
        "expected stderr to describe a parse error; got:\n{stderr}"
    );
}

#[test]
fn captain_dispatch_help_lists_dry_run_flag() {
    let out = Command::new(captain_bin())
        .args(["dispatch", "--help"])
        .output()
        .expect("spawn captain dispatch --help");
    assert!(
        out.status.success(),
        "captain dispatch --help should succeed; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--dry-run"),
        "expected `--dry-run` in help output:\n{stdout}"
    );
}
