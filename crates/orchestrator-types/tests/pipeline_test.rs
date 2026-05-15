use orchestrator_types::{TaskShape, ValidatorPipeline};

#[test]
fn pipeline_serialization_snake_case() {
    let cases = [
        (ValidatorPipeline::Refactor, "\"refactor\""),
        (ValidatorPipeline::BugFix, "\"bug_fix\""),
        (ValidatorPipeline::Mechanical, "\"mechanical\""),
        (ValidatorPipeline::Feature, "\"feature\""),
        (ValidatorPipeline::Substrate, "\"substrate\""),
        (ValidatorPipeline::Triage, "\"triage\""),
        (ValidatorPipeline::TrivialDoc, "\"trivial_doc\""),
    ];
    for (variant, wire) in cases {
        let json = serde_json::to_string(&variant).expect("ser");
        assert_eq!(
            json, wire,
            "variant {variant:?} did not serialize to {wire}"
        );
    }
}

#[test]
fn pipeline_deserialization_roundtrip() {
    let cases = [
        ("\"refactor\"", ValidatorPipeline::Refactor),
        ("\"bug_fix\"", ValidatorPipeline::BugFix),
        ("\"mechanical\"", ValidatorPipeline::Mechanical),
        ("\"feature\"", ValidatorPipeline::Feature),
        ("\"substrate\"", ValidatorPipeline::Substrate),
        ("\"triage\"", ValidatorPipeline::Triage),
        ("\"trivial_doc\"", ValidatorPipeline::TrivialDoc),
    ];
    for (wire, expected) in cases {
        let got: ValidatorPipeline = serde_json::from_str(wire).expect("de");
        assert_eq!(
            got, expected,
            "wire {wire} did not deserialize to {expected:?}"
        );
    }
}

#[test]
fn pipeline_alias_debug_deserializes_to_bug_fix() {
    let got: ValidatorPipeline = serde_json::from_str("\"debug\"").expect("de");
    assert_eq!(got, ValidatorPipeline::BugFix);
}

#[test]
fn pipeline_alias_new_feature_deserializes_to_feature() {
    let got: ValidatorPipeline = serde_json::from_str("\"new_feature\"").expect("de");
    assert_eq!(got, ValidatorPipeline::Feature);
}

#[test]
fn pipeline_alias_audit_deserializes_to_triage() {
    let got: ValidatorPipeline = serde_json::from_str("\"audit\"").expect("de");
    assert_eq!(got, ValidatorPipeline::Triage);
}

#[test]
fn pipeline_alias_doc_deserializes_to_trivial_doc() {
    let got: ValidatorPipeline = serde_json::from_str("\"doc\"").expect("de");
    assert_eq!(got, ValidatorPipeline::TrivialDoc);
}

#[test]
fn pipeline_rejects_unknown_string() {
    assert!(serde_json::from_str::<ValidatorPipeline>("\"made_up\"").is_err());
}

#[test]
fn taskshape_to_pipeline_mapping() {
    let cases = [
        (TaskShape::TrivialDoc, ValidatorPipeline::TrivialDoc),
        (TaskShape::TrivialMechanical, ValidatorPipeline::Mechanical),
        (TaskShape::Mechanical, ValidatorPipeline::Mechanical),
        (TaskShape::BugFix, ValidatorPipeline::BugFix),
        (TaskShape::Feature, ValidatorPipeline::Feature),
        (TaskShape::Migration, ValidatorPipeline::Feature),
        (TaskShape::Portage, ValidatorPipeline::Refactor),
        (TaskShape::Sweep, ValidatorPipeline::Refactor),
        (TaskShape::Triage, ValidatorPipeline::Triage),
    ];
    for (shape, expected) in cases {
        let got: ValidatorPipeline = shape.into();
        assert_eq!(
            got, expected,
            "{shape:?} did not map to expected pipeline {expected:?}"
        );
    }
}

#[test]
fn taskshape_to_pipeline_is_total() {
    let _: ValidatorPipeline = TaskShape::TrivialDoc.into();
    let _: ValidatorPipeline = TaskShape::TrivialMechanical.into();
    let _: ValidatorPipeline = TaskShape::Mechanical.into();
    let _: ValidatorPipeline = TaskShape::BugFix.into();
    let _: ValidatorPipeline = TaskShape::Feature.into();
    let _: ValidatorPipeline = TaskShape::Migration.into();
    let _: ValidatorPipeline = TaskShape::Portage.into();
    let _: ValidatorPipeline = TaskShape::Sweep.into();
    let _: ValidatorPipeline = TaskShape::Triage.into();
}
