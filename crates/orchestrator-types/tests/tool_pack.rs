use orchestrator_types::ToolPack;

#[test]
fn tool_pack_round_trip_json() {
    let p = ToolPack {
        name: "rust-cargo".into(),
        version: 1,
        binaries: vec!["cargo".into(), "rustup".into()],
        container_bootstrap: vec![
            "curl --proto '=https' -sSf https://sh.rustup.rs | sh -s -- -y".into(),
            "export PATH=\"$HOME/.cargo/bin:$PATH\"".into(),
        ],
        allowed_tools_added: vec!["Bash(cargo:*)".into(), "Read".into()],
        system_prompt_fragment: Some(
            "## Rust\n\nUse `cargo fmt` and `cargo clippy` before committing.".into(),
        ),
    };
    let v = serde_json::to_value(&p).expect("ser");
    let back: ToolPack = serde_json::from_value(v).expect("de");
    assert_eq!(p, back);
}

#[test]
fn tool_pack_defaults_empty_optional_fields() {
    let json = r#"{ "name": "minimal", "version": 1 }"#;
    let p: ToolPack = serde_json::from_str(json).expect("de");
    assert_eq!(p.name, "minimal");
    assert_eq!(p.version, 1);
    assert!(p.binaries.is_empty());
    assert!(p.container_bootstrap.is_empty());
    assert!(p.allowed_tools_added.is_empty());
    assert!(p.system_prompt_fragment.is_none());
}

#[test]
fn tool_pack_rejects_unknown_field() {
    let json = r#"{ "name": "x", "version": 1, "unknown_field": "nope" }"#;
    let r: Result<ToolPack, _> = serde_json::from_str(json);
    assert!(
        r.is_err(),
        "deny_unknown_fields must reject extra keys, got: {r:?}"
    );
}

#[test]
fn tool_pack_jsonschema_includes_required_fields() {
    let schema = schemars::schema_for!(ToolPack);
    let v = serde_json::to_value(&schema).expect("schema must serialize");
    let required = v
        .get("required")
        .and_then(|r| r.as_array())
        .expect("ToolPack schema must declare a `required` array");
    let names: Vec<&str> = required.iter().filter_map(|x| x.as_str()).collect();
    assert!(
        names.contains(&"name"),
        "required must include `name` (got: {names:?})"
    );
    assert!(
        names.contains(&"version"),
        "required must include `version` (got: {names:?})"
    );
}
