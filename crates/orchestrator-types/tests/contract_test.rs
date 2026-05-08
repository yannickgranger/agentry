use orchestrator_types::{Assertion, AssertionAnchor, AssertionId, BriefId, Contract};
use std::path::PathBuf;

#[test]
fn assertion_anchor_cfdb_roundtrip() {
    let a = AssertionAnchor::Cfdb {
        qname: "orchestrator_types::contract::Contract".into(),
    };
    let json = serde_json::to_string(&a).expect("ser");
    assert!(json.contains("\"kind\":\"cfdb\""));
    let back: AssertionAnchor = serde_json::from_str(&json).expect("de");
    assert_eq!(a, back);
}

#[test]
fn assertion_anchor_spec_concept_roundtrip() {
    let a = AssertionAnchor::SpecConcept {
        path: PathBuf::from("specs/concepts/brief_contract.md"),
        section: "Contract".into(),
    };
    let json = serde_json::to_string(&a).expect("ser");
    assert!(json.contains("\"kind\":\"spec_concept\""));
    let back: AssertionAnchor = serde_json::from_str(&json).expect("de");
    assert_eq!(a, back);
}

#[test]
fn assertion_anchor_behavior_roundtrip() {
    let a = AssertionAnchor::Behavior {
        live_target: "https://agency.lab:3000/api/v1/version".into(),
    };
    let json = serde_json::to_string(&a).expect("ser");
    assert!(json.contains("\"kind\":\"behavior\""));
    let back: AssertionAnchor = serde_json::from_str(&json).expect("de");
    assert_eq!(a, back);
}

#[test]
fn contract_roundtrip() {
    let c = Contract {
        brief_id: BriefId("brf_test".into()),
        assertions: vec![
            Assertion {
                id: AssertionId("A1".into()),
                prose: "Contract type exists in orchestrator-types".into(),
                anchor: AssertionAnchor::Cfdb {
                    qname: "orchestrator_types::contract::Contract".into(),
                },
            },
            Assertion {
                id: AssertionId("A2".into()),
                prose: "Contract concept declared in graph-specs".into(),
                anchor: AssertionAnchor::SpecConcept {
                    path: PathBuf::from("specs/concepts/brief_contract.md"),
                    section: "Contract".into(),
                },
            },
        ],
        precursor_artifacts: vec![PathBuf::from("crates/orchestrator-types/src/contract.rs")],
    };
    let json = serde_json::to_string(&c).expect("ser");
    let back: Contract = serde_json::from_str(&json).expect("de");
    assert_eq!(c, back);
}

#[test]
fn assertion_anchor_rejects_unknown_kind() {
    let json = r#"{ "kind": "made_up", "qname": "x" }"#;
    assert!(serde_json::from_str::<AssertionAnchor>(json).is_err());
}

#[test]
fn contract_rejects_unknown_field() {
    let json = r#"{
        "brief_id": "brf_test",
        "assertions": [],
        "precursor_artifacts": [],
        "extra_top_level_field": true
    }"#;
    assert!(serde_json::from_str::<Contract>(json).is_err());
}
