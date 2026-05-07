//! Integration tests for `orchestrator role` CLI handlers.

use orchestrator_runtime::{cli_roles::validate, redis_io};
use orchestrator_types::{
    AgentRole, PackageManager, PermitScope, RoleName, SubstrateClass, ToolAllowlist,
};
use std::io::Write;

fn minimal_role() -> AgentRole {
    AgentRole {
        name: RoleName("validate-probe".into()),
        version: 7,
        model: None,
        system_prompt: None,
        image: "alpine:3.21".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: "#!/usr/bin/env bash\nexit 0\n".into(),
        exitpoint_script: None,
        binaries: vec![],
        mcp_servers: vec![],
        tool_allowlist: ToolAllowlist::default(),
        allowed_tools: None,
        permit_scope: PermitScope::default(),
        passthru_env: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
        extra_bootstrap: vec![],
        tool_packs: vec![],
    }
}

fn redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

#[test]
fn schema_outputs_valid_json() {
    let schema = schemars::schema_for!(AgentRole);
    let v = serde_json::to_value(&schema).expect("schema must serialize as JSON");
    let obj = v.as_object().expect("schema root must be a JSON object");
    assert!(!obj.is_empty(), "schema root object must not be empty");
    assert!(
        obj.contains_key("properties"),
        "AgentRole schema must expose a `properties` map (got keys: {:?})",
        obj.keys().collect::<Vec<_>>()
    );
}

#[test]
fn schema_includes_image_field() {
    let schema = schemars::schema_for!(AgentRole);
    let v = serde_json::to_value(&schema).expect("schema must serialize as JSON");
    let props = v
        .get("properties")
        .and_then(|p| p.as_object())
        .expect("AgentRole schema must have a `properties` object");
    assert!(
        props.contains_key("image"),
        "AgentRole schema must include `image` (got: {:?})",
        props.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL) — connection unused but signature requires one"]
async fn validate_accepts_minimal_role() {
    let Some(url) = redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");
    let role = minimal_role();
    let body = serde_json::to_string_pretty(&role).expect("ser");
    let mut f = tempfile::NamedTempFile::new().expect("tmp");
    f.write_all(body.as_bytes()).expect("write");

    let r = validate(&mut conn, f.path()).await;
    assert!(r.is_ok(), "minimal valid role must validate clean");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL) — connection unused but signature requires one"]
async fn validate_rejects_unknown_field() {
    let Some(url) = redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");
    let role = minimal_role();
    let mut value = serde_json::to_value(&role).expect("to_value");
    if let serde_json::Value::Object(ref mut map) = value {
        map.insert("__bogus__".into(), serde_json::Value::String("nope".into()));
    }
    let body = serde_json::to_string_pretty(&value).expect("ser");
    let mut f = tempfile::NamedTempFile::new().expect("tmp");
    f.write_all(body.as_bytes()).expect("write");

    let r = validate(&mut conn, f.path()).await;
    assert!(
        r.is_err(),
        "unknown top-level field must be rejected by deny_unknown_fields"
    );
}
