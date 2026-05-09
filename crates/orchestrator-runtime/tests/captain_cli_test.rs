//! Integration tests for the `captain new-brief` subcommand.
//!
//! Tests run the compiled `captain` binary as a subprocess (cargo sets the
//! `CARGO_BIN_EXE_captain` env var for integration tests in this crate) and
//! parse stdout back into `orchestrator_types::Brief`. The point of these
//! tests is to lock in that the emitted JSON deserializes via the current
//! Brief schema with `kind` + `contract` at the correct top-level fields —
//! that is the exact failure mode the captain CLI exists to prevent.

use orchestrator_types::{AssertionAnchor, Brief, BriefId, TaskShape};
use std::process::Command;

fn captain_bin() -> &'static str {
    env!("CARGO_BIN_EXE_captain")
}

fn run_new_brief(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(captain_bin());
    cmd.arg("new-brief");
    cmd.args(args);
    cmd.output().expect("spawn captain new-brief")
}

fn parse_stdout(out: &std::process::Output) -> Brief {
    assert!(
        out.status.success(),
        "captain exited non-zero: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::str::from_utf8(&out.stdout).expect("stdout is utf-8");
    serde_json::from_str::<Brief>(text)
        .unwrap_or_else(|e| panic!("parse Brief from stdout failed: {e}\nstdout was:\n{text}"))
}

#[test]
fn captain_new_brief_emits_valid_brief_json_for_trivial_doc() {
    let out = run_new_brief(&[
        "--kind",
        "trivial-doc",
        "--target-repo",
        "yg/agentry",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "test",
        "--issue-title",
        "test",
    ]);
    let brief = parse_stdout(&out);
    assert_eq!(brief.kind, Some(TaskShape::TrivialDoc));
    assert!(
        brief.contract.is_none(),
        "TrivialDoc must not stub a contract (requires_contract = false)"
    );
    assert_eq!(brief.topology.name, "agentry-bugfix-v0");
    assert_eq!(brief.topology.version, 1);
}

#[test]
fn captain_new_brief_emits_stub_contract_for_mechanical() {
    let out = run_new_brief(&[
        "--kind",
        "mechanical",
        "--target-repo",
        "yg/agentry",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "test",
        "--issue-title",
        "test",
    ]);
    let brief = parse_stdout(&out);
    assert_eq!(brief.kind, Some(TaskShape::Mechanical));
    let contract = brief
        .contract
        .as_ref()
        .expect("Mechanical must stub a contract");
    assert_eq!(contract.assertions.len(), 1);
    let anchor = &contract.assertions[0].anchor;
    match anchor {
        AssertionAnchor::Cfdb { qname } => {
            assert_eq!(qname, "TODO::replace_with_real_qname");
        }
        other => panic!("expected Cfdb TODO anchor, got {other:?}"),
    }
}

#[test]
fn captain_new_brief_uses_provided_id() {
    let out = run_new_brief(&[
        "--kind",
        "trivial-doc",
        "--target-repo",
        "yg/agentry",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "test",
        "--issue-title",
        "test",
        "--id",
        "custom-id-string",
    ]);
    let brief = parse_stdout(&out);
    assert_eq!(brief.id, BriefId("custom-id-string".into()));
}

#[test]
fn new_brief_default_emits_single_cfdb_anchor() {
    let out = run_new_brief(&[
        "--kind",
        "mechanical",
        "--target-repo",
        "yg/agentry",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "test",
        "--issue-title",
        "test",
    ]);
    let brief = parse_stdout(&out);
    let contract = brief
        .contract
        .as_ref()
        .expect("Mechanical must stub a contract");
    assert_eq!(
        contract.assertions.len(),
        1,
        "default contract must have exactly one assertion"
    );
    match &contract.assertions[0].anchor {
        AssertionAnchor::Cfdb { .. } => {}
        other => panic!("expected Cfdb anchor in default mode, got {other:?}"),
    }
}

#[test]
fn new_brief_bootstrap_emits_three_behavior_anchors() {
    let out = run_new_brief(&[
        "--kind",
        "mechanical",
        "--target-repo",
        "yg/agentry",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "test",
        "--issue-title",
        "test",
        "--bootstrap",
    ]);
    let brief = parse_stdout(&out);
    let contract = brief
        .contract
        .as_ref()
        .expect("bootstrap mode must stub a contract for kinds that require one");
    assert_eq!(
        contract.assertions.len(),
        3,
        "bootstrap contract must have exactly three assertions"
    );
    let expected_ids = ["A1", "A2", "A3"];
    for (idx, assertion) in contract.assertions.iter().enumerate() {
        assert_eq!(
            assertion.id.0, expected_ids[idx],
            "assertion at index {idx} has wrong id"
        );
        match &assertion.anchor {
            AssertionAnchor::Behavior { .. } => {}
            other => panic!("expected Behavior anchor at index {idx}, got {other:?}"),
        }
    }
}

#[test]
fn new_brief_bootstrap_assertions_have_behavior_live_targets() {
    let out = run_new_brief(&[
        "--kind",
        "mechanical",
        "--target-repo",
        "yg/agentry",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "test",
        "--issue-title",
        "test",
        "--bootstrap",
    ]);
    let brief = parse_stdout(&out);
    let contract = brief
        .contract
        .as_ref()
        .expect("bootstrap mode must stub a contract for kinds that require one");
    for assertion in &contract.assertions {
        match &assertion.anchor {
            AssertionAnchor::Behavior { live_target } => {
                assert!(
                    !live_target.is_empty(),
                    "behavior anchor live_target must be non-empty for {}",
                    assertion.id
                );
            }
            other => panic!(
                "expected Behavior anchor for {}, got {other:?}",
                assertion.id
            ),
        }
    }
}

#[test]
fn captain_new_brief_rejects_unknown_kind() {
    let out = run_new_brief(&[
        "--kind",
        "made-up",
        "--target-repo",
        "yg/agentry",
        "--topology",
        "agentry-bugfix-v0:1",
        "--pr-title",
        "test",
        "--issue-title",
        "test",
    ]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for unknown --kind, got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
