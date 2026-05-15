#![allow(clippy::expect_used, clippy::unwrap_used)]
use orchestrator_types::brief::{Brief, BriefId, RedeployTarget};
use orchestrator_types::{Budget, EscalationMode, TaskShape, VersionedRef};
use serde_json::json;

fn empty_brief() -> Brief {
    Brief {
        id: BriefId("test".into()),
        project: None,
        topology: VersionedRef::new("agentry-self-host-v0", 1),
        payload: json!({}),
        kind: Some(TaskShape::Feature),
        contract: None,
        budget: Budget::default(),
        escalation: EscalationMode::Autonomous,
        parent_brief: None,
        cohort_labels: vec![],
        submitted_by: "test".into(),
        submitted_at: orchestrator_types::now(),
        redeploy_required: vec![],
    }
}

#[test]
fn empty_redeploy_required_omitted_from_serialization() {
    let brief = empty_brief();
    let json = serde_json::to_string(&brief).unwrap();
    assert!(
        !json.contains("redeploy_required"),
        "empty redeploy_required should be skipped in serialization for backward compat; got json:\n{json}"
    );
}

#[test]
fn brief_without_redeploy_required_field_deserializes() {
    // Wire form lacking the field — represents older briefs / external producers.
    let wire = r#"{
        "id": "test",
        "project": null,
        "topology": {"name": "agentry-self-host-v0", "version": 1},
        "payload": {},
        "kind": "feature",
        "contract": null,
        "budget": {"max_wall_seconds": 900},
        "escalation": "autonomous",
        "parent_brief": null,
        "cohort_labels": [],
        "submitted_by": "test",
        "submitted_at": "2026-05-09T00:00:00Z"
    }"#;
    let brief: Brief = serde_json::from_str(wire).expect("legacy wire form parses");
    assert!(brief.redeploy_required.is_empty());
}

#[test]
fn redeploy_targets_round_trip_with_snake_case_wire_form() {
    let mut brief = empty_brief();
    brief.redeploy_required = vec![RedeployTarget::Daemon, RedeployTarget::CaptainCli];
    let json = serde_json::to_string(&brief).unwrap();
    assert!(
        json.contains(r#""redeploy_required":["daemon","captain_cli"]"#),
        "wire form should be snake_case array; got: {json}"
    );
    let round: Brief = serde_json::from_str(&json).unwrap();
    assert_eq!(round.redeploy_required.len(), 2);
    assert!(round.redeploy_required.contains(&RedeployTarget::Daemon));
    assert!(round
        .redeploy_required
        .contains(&RedeployTarget::CaptainCli));
}

#[test]
fn variant_coverage_compiles() {
    // Compile-time check that adding a future variant forces this test
    // to be updated. Match must be exhaustive.
    fn coverage(t: RedeployTarget) -> &'static str {
        match t {
            RedeployTarget::Daemon => "daemon",
            RedeployTarget::OrchestratorCli => "orchestrator_cli",
            RedeployTarget::CaptainCli => "captain_cli",
        }
    }
    assert_eq!(coverage(RedeployTarget::Daemon), "daemon");
}
