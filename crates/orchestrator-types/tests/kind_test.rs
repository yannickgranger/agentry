use orchestrator_types::kind::BriefKind;

#[test]
fn kind_serialization_kebab_case() {
    let cases = [
        (BriefKind::TrivialDoc, "\"trivial-doc\""),
        (BriefKind::TrivialMechanical, "\"trivial-mechanical\""),
        (BriefKind::Mechanical, "\"mechanical\""),
        (BriefKind::BugFix, "\"bug-fix\""),
        (BriefKind::Feature, "\"feature\""),
        (BriefKind::Migration, "\"migration\""),
        (BriefKind::Portage, "\"portage\""),
        (BriefKind::Sweep, "\"sweep\""),
        (BriefKind::Triage, "\"triage\""),
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
        ("\"trivial-doc\"", BriefKind::TrivialDoc),
        ("\"trivial-mechanical\"", BriefKind::TrivialMechanical),
        ("\"mechanical\"", BriefKind::Mechanical),
        ("\"bug-fix\"", BriefKind::BugFix),
        ("\"feature\"", BriefKind::Feature),
        ("\"migration\"", BriefKind::Migration),
        ("\"portage\"", BriefKind::Portage),
        ("\"sweep\"", BriefKind::Sweep),
        ("\"triage\"", BriefKind::Triage),
    ];
    for (wire, expected) in cases {
        let got: BriefKind = serde_json::from_str(wire).expect("de");
        assert_eq!(
            got, expected,
            "wire {wire} did not deserialize to {expected:?}"
        );
    }
}

#[test]
fn kind_rejects_unknown_string() {
    assert!(serde_json::from_str::<BriefKind>("\"made-up\"").is_err());
}

#[test]
fn kind_requires_contract_trivial() {
    assert!(!BriefKind::TrivialDoc.requires_contract());
    assert!(!BriefKind::TrivialMechanical.requires_contract());
}

#[test]
fn kind_requires_contract_non_trivial() {
    for v in [
        BriefKind::Mechanical,
        BriefKind::BugFix,
        BriefKind::Feature,
        BriefKind::Migration,
        BriefKind::Portage,
        BriefKind::Sweep,
        BriefKind::Triage,
    ] {
        assert!(v.requires_contract(), "{v:?} should require a contract");
    }
}

#[test]
fn kind_is_copy() {
    let a = BriefKind::Mechanical;
    let b = a;
    assert_eq!(a, b);
}
