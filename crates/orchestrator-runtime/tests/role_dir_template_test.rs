//! Unit tests for the loader template-substitution surface
//! (`expand_string_templates`, `expand_role_templates`, and the
//! `TemplateContext` they consume). Lives in `tests/` rather than inline
//! under `src/` to honor the `arch-ban-inline-cfg-test-in-src` ban rule.

use orchestrator_runtime::role_dir_loader::{
    expand_role_templates, expand_string_templates, TemplateContext,
};
use orchestrator_types::{
    AgentRole, Mount, PackageManager, PermitScope, RoleName, SubstrateClass, ToolAllowlist,
};

fn ctx_with(
    home: &str,
    forge_net_allow: &str,
    forge_write_permits: Vec<String>,
    sccache_net_allow: Option<String>,
) -> TemplateContext {
    TemplateContext {
        home: home.into(),
        forge_net_allow: forge_net_allow.into(),
        forge_write_permits,
        sccache_net_allow,
    }
}

fn minimal_role() -> AgentRole {
    AgentRole {
        name: RoleName("t".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "alpine:3.21".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: "exit 0".into(),
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
    }
}

#[test]
fn expands_tilde_in_mount_source() {
    let ctx = ctx_with("/home/u", "net:allow:agency.lab", vec![], None);
    assert_eq!(
        expand_string_templates("~/.claude/.credentials.json", &ctx),
        "/home/u/.claude/.credentials.json"
    );
    assert_eq!(expand_string_templates("~", &ctx), "~");
    assert_eq!(expand_string_templates("/abs/path", &ctx), "/abs/path");
}

#[test]
fn expand_string_templates_substitutes_home() {
    let ctx = ctx_with("/home/u", "net:allow:agency.lab", vec![], None);
    assert_eq!(
        expand_string_templates("${HOME}/.claude", &ctx),
        "/home/u/.claude"
    );
}

#[test]
fn expand_string_templates_substitutes_forge_net_allow() {
    let ctx = ctx_with("/home/u", "net:allow:forge.example.com", vec![], None);
    assert_eq!(
        expand_string_templates("${FORGE_NET_ALLOW}", &ctx),
        "net:allow:forge.example.com"
    );
    assert_eq!(
        expand_string_templates("prefix-${FORGE_NET_ALLOW}-suffix", &ctx),
        "prefix-net:allow:forge.example.com-suffix"
    );
}

#[test]
fn expand_string_templates_substitutes_sccache_net_allow() {
    let ctx_some = ctx_with(
        "/home/u",
        "net:allow:agency.lab",
        vec![],
        Some("net:allow:sccache-redis".into()),
    );
    assert_eq!(
        expand_string_templates("${SCCACHE_NET_ALLOW}", &ctx_some),
        "net:allow:sccache-redis"
    );

    let ctx_none = ctx_with("/home/u", "net:allow:agency.lab", vec![], None);
    assert_eq!(
        expand_string_templates("${SCCACHE_NET_ALLOW}", &ctx_none),
        ""
    );
}

#[test]
fn expand_role_templates_spreads_forge_write_permits() {
    let ctx = ctx_with(
        "/home/u",
        "net:allow:agency.lab",
        vec!["forge:write:a/*".into(), "forge:write:b/*".into()],
        None,
    );
    let mut role = minimal_role();
    role.permit_scope = PermitScope(vec![
        "fs:read:/x".into(),
        "${FORGE_WRITE_PERMITS}".into(),
        "fs:write:/y".into(),
    ]);
    expand_role_templates(&mut role, &ctx);
    assert_eq!(
        role.permit_scope.0,
        vec![
            "fs:read:/x".to_string(),
            "forge:write:a/*".to_string(),
            "forge:write:b/*".to_string(),
            "fs:write:/y".to_string(),
        ]
    );
}

#[test]
fn expand_role_templates_drops_sole_sccache_token_when_none() {
    let ctx = ctx_with("/home/u", "net:allow:agency.lab", vec![], None);
    let mut role = minimal_role();
    role.permit_scope = PermitScope(vec!["fs:read:/x".into(), "${SCCACHE_NET_ALLOW}".into()]);
    expand_role_templates(&mut role, &ctx);
    assert_eq!(role.permit_scope.0, vec!["fs:read:/x".to_string()]);
}

#[test]
fn expand_role_templates_preserves_unrelated_permits() {
    let ctx = ctx_with(
        "/home/u",
        "net:allow:agency.lab",
        vec!["forge:write:a/*".into()],
        Some("net:allow:sccache".into()),
    );
    let original = vec![
        "fs:read:/workspace/**".to_string(),
        "fs:write:/workspace/.cargo".to_string(),
        "net:deny:*".to_string(),
    ];
    let mut role = minimal_role();
    role.permit_scope = PermitScope(original.clone());
    expand_role_templates(&mut role, &ctx);
    assert_eq!(role.permit_scope.0, original);
}

#[test]
fn expand_role_templates_walks_mount_sources() {
    let ctx = ctx_with("/home/u", "net:allow:agency.lab", vec![], None);
    let mut role = minimal_role();
    role.mounts = vec![Mount {
        source: "~/.claude/.credentials.json".into(),
        target: "/root/.claude/.credentials.json".into(),
        readonly: true,
    }];
    expand_role_templates(&mut role, &ctx);
    assert_eq!(role.mounts[0].source, "/home/u/.claude/.credentials.json");
}
