use orchestrator_types::kind::TaskShape;

#[test]
fn kind_serialization_kebab_case() {
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
    for (variant, wire) in cases {
        let json = serde_json::to_string(&variant).expect("ser");
        assert_eq!(
            json, wire,
            "variant {variant:?} did not serialize to {wire}"
        );
    }
}

#[test]
fn kind_deserialization_roundtrip() {
    let cases = [
        ("\"trivial-doc\"", TaskShape::TrivialDoc),
        ("\"trivial-mechanical\"", TaskShape::TrivialMechanical),
        ("\"mechanical\"", TaskShape::Mechanical),
        ("\"bug-fix\"", TaskShape::BugFix),
        ("\"feature\"", TaskShape::Feature),
        ("\"migration\"", TaskShape::Migration),
        ("\"portage\"", TaskShape::Portage),
        ("\"sweep\"", TaskShape::Sweep),
        ("\"triage\"", TaskShape::Triage),
    ];
    for (wire, expected) in cases {
        let got: TaskShape = serde_json::from_str(wire).expect("de");
        assert_eq!(
            got, expected,
            "wire {wire} did not deserialize to {expected:?}"
        );
    }
}

#[test]
fn kind_rejects_unknown_string() {
    assert!(serde_json::from_str::<TaskShape>("\"made-up\"").is_err());
}

#[test]
fn kind_requires_contract_trivial() {
    assert!(!TaskShape::TrivialDoc.requires_contract());
    assert!(!TaskShape::TrivialMechanical.requires_contract());
}

#[test]
fn kind_requires_contract_non_trivial() {
    for v in [
        TaskShape::Mechanical,
        TaskShape::BugFix,
        TaskShape::Feature,
        TaskShape::Migration,
        TaskShape::Portage,
        TaskShape::Sweep,
        TaskShape::Triage,
    ] {
        assert!(v.requires_contract(), "{v:?} should require a contract");
    }
}

#[test]
fn kind_is_copy() {
    let a = TaskShape::Mechanical;
    let b = a;
    assert_eq!(a, b);
}
