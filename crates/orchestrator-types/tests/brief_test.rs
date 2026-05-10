use orchestrator_types::{
    now, Assertion, AssertionAnchor, AssertionId, Brief, BriefId, Contract, EscalationMode,
    TaskShape, VersionedRef,
};
use serde_json::json;

fn brief_with_payload(payload: serde_json::Value) -> Brief {
    Brief::new("tests", VersionedRef::new("topology", 1), payload)
}

#[test]
fn target_repo_returns_some_for_valid_payload() {
    let b = brief_with_payload(json!({ "target_repo": "yg/agentry" }));
    let tr = b.target_repo().expect("must parse");
    assert_eq!(tr.owner(), "yg");
    assert_eq!(tr.repo(), "agentry");
}

#[test]
fn target_repo_returns_none_when_field_missing() {
    let b = brief_with_payload(json!({}));
    assert!(b.target_repo().is_none());
}

#[test]
fn target_repo_returns_none_when_payload_null() {
    let b = brief_with_payload(serde_json::Value::Null);
    assert!(b.target_repo().is_none());
}

#[test]
fn target_repo_returns_none_for_malformed_string() {
    let b = brief_with_payload(json!({ "target_repo": "yg/agentry@evil" }));
    assert!(b.target_repo().is_none());
}

#[test]
fn brief_roundtrip_json() {
    let b = Brief::new(
        "user@example.com",
        VersionedRef::new("echo-team", 1),
        json!({"kind": "echo", "msg": "hello"}),
    )
    .with_project("qbot-core")
    .with_escalation(EscalationMode::Autonomous);
    let s = serde_json::to_string(&b).expect("serialize");
    let back: Brief = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(b, back);
    assert!(s.contains("brf_"), "brief id should have prefix");
}

#[test]
fn brief_id_is_prefixed() {
    let id = BriefId::fresh();
    assert!(id.0.starts_with("brf_"));
}

#[test]
fn default_escalation_is_supervised() {
    assert_eq!(EscalationMode::default(), EscalationMode::Supervised);
}

#[test]
fn brief_kind_roundtrip_serializes_kebab_case() {
    let mut b = Brief::new(
        "user@example.com",
        VersionedRef::new("echo-team", 1),
        json!({"msg": "hi"}),
    );
    b.kind = Some(TaskShape::Mechanical);
    let s = serde_json::to_string(&b).expect("serialize");
    assert!(
        s.contains("\"kind\":\"mechanical\""),
        "expected kebab-case kind in {s}"
    );
    let back: Brief = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(back.kind, Some(TaskShape::Mechanical));
}

#[test]
fn brief_without_kind_field_deserializes_to_none() {
    let raw = json!({
        "id": "brf_test",
        "project": null,
        "topology": { "name": "echo-team", "version": 1 },
        "payload": { "msg": "hi" },
        "submitted_by": "tester",
        "submitted_at": now(),
    });
    let s = serde_json::to_string(&raw).expect("serialize value");
    let b: Brief = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(b.kind, None);
}

#[test]
fn payload_default_contract_is_none() {
    let b = Brief::new(
        "user@example.com",
        VersionedRef::new("echo-team", 1),
        json!({"msg": "hi"}),
    );
    let s = serde_json::to_string(&b).expect("serialize");
    let back: Brief = serde_json::from_str(&s).expect("deserialize");
    assert!(back.contract.is_none());
}

#[test]
fn payload_with_contract_roundtrips() {
    let mut b = Brief::new(
        "user@example.com",
        VersionedRef::new("echo-team", 1),
        json!({"msg": "hi"}),
    );
    b.contract = Some(Contract {
        brief_id: b.id.clone(),
        assertions: vec![Assertion {
            id: AssertionId("A1".into()),
            prose: "structural anchor in cfdb".into(),
            anchor: AssertionAnchor::Cfdb {
                qname: "foo::bar".into(),
            },
        }],
        precursor_artifacts: vec![],
    });
    let s = serde_json::to_string(&b).expect("serialize");
    let back: Brief = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(b, back);
}

#[test]
fn payload_rejects_unknown_field_on_contract() {
    let raw = json!({
        "id": "brf_test",
        "project": null,
        "topology": { "name": "echo-team", "version": 1 },
        "payload": { "msg": "hi" },
        "contract": {
            "brief_id": "brf_test",
            "assertions": [],
            "precursor_artifacts": [],
            "extra_top_level_field": true
        },
        "submitted_by": "tester",
        "submitted_at": now(),
    });
    let s = serde_json::to_string(&raw).expect("serialize value");
    assert!(serde_json::from_str::<Brief>(&s).is_err());
}

#[test]
fn brief_kind_variants_serialize_kebab_case() {
    let cases = [
        (TaskShape::TrivialDoc, "\"trivial-doc\""),
        (TaskShape::TrivialMechanical, "\"trivial-mechanical\""),
        (TaskShape::Mechanical, "\"mechanical\""),
        (TaskShape::BugFix, "\"bug-fix\""),
        (TaskShape::Feature, "\"feature\""),
        (TaskShape::Migration, "\"migration\""),
        (TaskShape::Portage, "\"portage\""),
        (TaskShape::Sweep, "\"sweep\""),
        (TaskShape::Triage, "\"triage\""),
    ];
    for (k, want) in cases {
        let s = serde_json::to_string(&k).expect("serialize");
        assert_eq!(s, want, "variant {k:?}");
    }
}
