//! Tests for `submit_shape_check`. The shape gate validates
//! `brief.payload.success_criteria` at `orchestrator submit` time for
//! topologies that consume it (planner-v0, verify-v0). Brief 84b-2.

use orchestrator_runtime::submit_shape_check::{check_brief, ShapeError};
use orchestrator_types::{Brief, BriefId, EscalationMode, VersionedRef};
use serde_json::json;

fn brief_with(topology: &str, payload: serde_json::Value) -> Brief {
    Brief {
        id: BriefId("brf_test".into()),
        project: None,
        topology: VersionedRef::new(topology, 1),
        payload,
        kind: None,
        contract: None,
        budget: Default::default(),
        escalation: EscalationMode::Autonomous,
        parent_brief: None,
        cohort_labels: Vec::new(),
        redeploy_required: vec![],
        submitted_by: "test".into(),
        submitted_at: chrono::Utc::now(),
    }
}

#[test]
fn rejects_empty_success_criteria_for_planner() {
    let b = brief_with("agentry-planner-v0", json!({"success_criteria": ""}));
    assert_eq!(check_brief(&b), Err(ShapeError::MissingOrEmpty));
}

#[test]
fn rejects_whitespace_only_success_criteria_for_planner() {
    let b = brief_with("agentry-planner-v0", json!({"success_criteria": "   \t  "}));
    assert_eq!(check_brief(&b), Err(ShapeError::MissingOrEmpty));
}

#[test]
fn rejects_missing_success_criteria_for_planner() {
    let b = brief_with("agentry-planner-v0", json!({}));
    assert_eq!(check_brief(&b), Err(ShapeError::MissingOrEmpty));
}

#[test]
fn rejects_missing_separator_for_planner() {
    let b = brief_with(
        "agentry-planner-v0",
        json!({"success_criteria": "wc -l < src/foo.rs"}),
    );
    assert_eq!(check_brief(&b), Err(ShapeError::MissingSeparator));
}

#[test]
fn rejects_empty_expected_for_planner() {
    let b = brief_with(
        "agentry-planner-v0",
        json!({"success_criteria": "wc -l < src/foo.rs : "}),
    );
    assert_eq!(check_brief(&b), Err(ShapeError::EmptyExpected));
}

#[test]
fn rejects_whitespace_only_expected_for_planner() {
    let b = brief_with(
        "agentry-planner-v0",
        json!({"success_criteria": "wc -l < src/foo.rs :    "}),
    );
    assert_eq!(check_brief(&b), Err(ShapeError::EmptyExpected));
}

#[test]
fn accepts_valid_criterion_for_planner() {
    let b = brief_with(
        "agentry-planner-v0",
        json!({"success_criteria": "wc -l < src/foo.rs : 0"}),
    );
    assert_eq!(check_brief(&b), Ok(()));
}

#[test]
fn accepts_valid_criterion_for_verify() {
    let b = brief_with(
        "agentry-verify-v0",
        json!({"success_criteria": "grep -c TODO src/foo.rs : 0"}),
    );
    assert_eq!(check_brief(&b), Ok(()));
}

#[test]
fn skips_check_for_non_consumer_topology() {
    let b = brief_with(
        "agentry-self-host-v0",
        json!({"acceptance": "cargo test --workspace"}),
    );
    assert_eq!(check_brief(&b), Ok(()));
}

#[test]
fn skips_check_for_non_consumer_even_with_bad_criterion() {
    let b = brief_with("agentry-self-host-v0", json!({"success_criteria": ""}));
    assert_eq!(check_brief(&b), Ok(()));
}

#[test]
fn error_messages_are_stable() {
    assert!(ShapeError::MissingOrEmpty.message().contains("required"));
    assert!(ShapeError::MissingSeparator
        .message()
        .contains("space-colon-space"));
    assert!(ShapeError::EmptyExpected.message().contains("'expected'"));
}
