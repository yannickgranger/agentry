use orchestrator_types::{
    parse_profile_toml, Profile, ProfileAcceptanceSection, ProfileMethodologySection,
    ProfileRoleSection,
};

#[test]
fn profile_round_trip_toml() {
    let p = Profile {
        coder: ProfileRoleSection {
            tool_packs: vec!["quality-fast".into(), "cfdb-grounding".into()],
        },
        reviewer: ProfileRoleSection {
            tool_packs: vec!["audit-split-brain".into(), "graph-specs-drift".into()],
        },
        acceptance: ProfileAcceptanceSection {
            default: Some(
                "cargo run -p quality-fast --bin quality-mech --release --quiet \
                 && bash scripts/arch-check.sh"
                    .into(),
            ),
        },
        methodology: ProfileMethodologySection {
            gates: vec![
                "discover".into(),
                "prescribe".into(),
                "prepare-issue".into(),
                "verify-issue".into(),
            ],
        },
    };
    let s = toml::to_string(&p).expect("ser");
    let back = parse_profile_toml(&s).expect("de");
    assert_eq!(p, back);
}

#[test]
fn profile_defaults_empty_sections() {
    let p = parse_profile_toml("").expect("empty profile parses");
    assert!(p.coder.tool_packs.is_empty());
    assert!(p.reviewer.tool_packs.is_empty());
    assert!(p.acceptance.default.is_none());
    assert!(p.methodology.gates.is_empty());
}

#[test]
fn profile_only_coder_section() {
    let doc = r#"
        [coder]
        tool_packs = ["x"]
    "#;
    let p = parse_profile_toml(doc).expect("de");
    assert_eq!(p.coder.tool_packs, vec!["x".to_string()]);
    assert!(p.reviewer.tool_packs.is_empty());
    assert!(p.acceptance.default.is_none());
    assert!(p.methodology.gates.is_empty());
}

#[test]
fn profile_rejects_unknown_section() {
    let doc = r#"
        [unknown_section]
        field = "x"
    "#;
    let r = parse_profile_toml(doc);
    assert!(
        r.is_err(),
        "deny_unknown_fields must reject unknown sections, got: {r:?}"
    );
}

#[test]
fn profile_rejects_unknown_field_in_section() {
    let doc = r#"
        [coder]
        unknown_field = "x"
    "#;
    let r = parse_profile_toml(doc);
    assert!(
        r.is_err(),
        "deny_unknown_fields must reject unknown fields in a section, got: {r:?}"
    );
}

#[test]
fn profile_jsonschema_required_fields() {
    let schema = schemars::schema_for!(Profile);
    let v = serde_json::to_value(&schema).expect("schema must serialize");
    assert!(
        v.get("title").is_some() || v.get("$schema").is_some(),
        "schema_for!(Profile) must produce a structured schema"
    );
}
