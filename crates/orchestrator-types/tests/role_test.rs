use orchestrator_types::role::{McpServer, PermitScope};
use orchestrator_types::{
    AgentRole, AllowedTools, PackageManager, RoleName, SubstrateClass, ToolAllowlist,
    WorkspaceMount,
};

#[test]
fn role_roundtrip_json() {
    let r = AgentRole {
        name: RoleName("coder-rust".into()),
        version: 3,
        model: Some("claude-opus-4-7".into()),
        system_prompt: Some("You are a Rust coder. Follow the contract.".into()),
        image: "alpine:3.21".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: "#!/usr/bin/env bash\necho hello\n".into(),
        exitpoint_script: None,
        binaries: vec!["git".into(), "curl".into()],
        mcp_servers: vec![McpServer {
            name: "ra-query".into(),
            image: Some("ghcr.io/yg/ra-query:v0.1".into()),
            binary: None,
        }],
        tool_allowlist: ToolAllowlist(vec!["read".into(), "edit".into(), "bash:cargo".into()]),
        allowed_tools: None,
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
            "net:deny:*".into(),
        ]),
        passthru_env: vec![],
        mounts: vec![],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: true,
        extra_bootstrap: vec![],
        tool_packs: vec![],
    };
    let s = serde_json::to_string_pretty(&r).expect("ser");
    let back: AgentRole = serde_json::from_str(&s).expect("de");
    assert_eq!(r, back);
}

#[test]
fn agent_role_roundtrips_with_extra_bootstrap() {
    let r = AgentRole {
        name: RoleName("coder-rust".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
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
        extra_bootstrap: vec!["rustup component add rustfmt clippy".into()],
        tool_packs: vec![],
    };
    let s = serde_json::to_string(&r).expect("ser");
    let back: AgentRole = serde_json::from_str(&s).expect("de");
    assert_eq!(r, back);
    assert_eq!(back.extra_bootstrap.len(), 1);
    assert_eq!(
        back.extra_bootstrap[0],
        "rustup component add rustfmt clippy"
    );
}

#[test]
fn agent_role_roundtrips_with_exitpoint() {
    let r = AgentRole {
        name: RoleName("coder-rust".into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "docker.io/library/rust:1.93".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apt,
        entrypoint_script: "#!/usr/bin/env bash\nexit 0\n".into(),
        exitpoint_script: Some("#!/usr/bin/env bash\nemit_done shipped\n".into()),
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
    };
    let s = serde_json::to_string(&r).expect("ser");
    let back: AgentRole = serde_json::from_str(&s).expect("de");
    assert_eq!(r, back);
    assert_eq!(
        back.exitpoint_script.as_deref(),
        Some("#!/usr/bin/env bash\nemit_done shipped\n")
    );
}

#[test]
fn workspace_mount_defaults_to_none() {
    // Old role JSON without the field must still deserialize — critical
    // for already-seeded roles (echo/naughty/speaker/etc.).
    let json = r##"{
        "name": "legacy",
        "version": 1,
        "model": null,
        "system_prompt": null,
        "image": "alpine:3.21",
        "package_manager": "apk",
        "entrypoint_script": "#!/usr/bin/env bash\nexit 0\n"
    }"##;
    let r: AgentRole = serde_json::from_str(json).expect("de");
    assert!(r.workspace_mount.is_none());
}

#[test]
fn default_substrate_is_podman() {
    assert_eq!(SubstrateClass::default(), SubstrateClass::Podman);
}

#[test]
fn allowlist_contains_works() {
    let a = ToolAllowlist(vec!["read".into(), "edit".into()]);
    assert!(a.contains("read"));
    assert!(!a.contains("write"));
}

#[test]
fn allowed_tools_roundtrip_json() {
    let a = AllowedTools(vec!["Bash(cargo fmt:*)".into(), "Read".into()]);
    let s = serde_json::to_string(&a).expect("ser");
    let back: AllowedTools = serde_json::from_str(&s).expect("de");
    assert_eq!(a, back);
}

fn role_with_allowed_tools(allowed: Option<AllowedTools>) -> AgentRole {
    AgentRole {
        name: RoleName("coder-rust".into()),
        version: 1,
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
        allowed_tools: allowed,
        permit_scope: PermitScope::default(),
        passthru_env: vec![],
        mounts: vec![],
        workspace_mount: None,
        sccache: false,
        extra_bootstrap: vec![],
        tool_packs: vec![],
    }
}

#[test]
fn agent_role_roundtrips_with_allowed_tools_some() {
    let r = role_with_allowed_tools(Some(AllowedTools(vec!["Read".into()])));
    let s = serde_json::to_string(&r).expect("ser");
    let back: AgentRole = serde_json::from_str(&s).expect("de");
    assert_eq!(r, back);
    assert_eq!(
        back.allowed_tools.as_ref().map(|a| a.0.as_slice()),
        Some(&["Read".to_string()][..])
    );
}

#[test]
fn agent_role_roundtrips_with_allowed_tools_none() {
    let r = role_with_allowed_tools(None);
    let s = serde_json::to_string(&r).expect("ser");
    // skip_serializing_if drops the field on the wire when None.
    assert!(!s.contains("allowed_tools"));
    let back: AgentRole = serde_json::from_str(&s).expect("de");
    assert_eq!(r, back);
    assert!(back.allowed_tools.is_none());
}
