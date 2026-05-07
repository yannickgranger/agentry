//! Tests for the pure pack-into-role merge function
//! `orchestrator_types::role::merge_role_with_packs`. Slice I/1c.

use orchestrator_types::role::{merge_role_with_packs, McpServer, PermitScope};
use orchestrator_types::{
    AgentRole, AllowedTools, PackageManager, RoleName, SubstrateClass, ToolAllowlist, ToolPack,
    WorkspaceMount,
};

fn role(
    binaries: Vec<String>,
    allowed_tools: Option<AllowedTools>,
    system_prompt: Option<String>,
    entrypoint_script: &str,
) -> AgentRole {
    AgentRole {
        name: RoleName("merge-fixture".into()),
        version: 1,
        model: None,
        system_prompt,
        image: "alpine:3.21".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: entrypoint_script.into(),
        exitpoint_script: None,
        binaries,
        mcp_servers: vec![McpServer {
            name: "ra-query".into(),
            image: None,
            binary: Some("/usr/local/bin/ra-query".into()),
        }],
        tool_allowlist: ToolAllowlist(vec!["read".into()]),
        allowed_tools,
        permit_scope: PermitScope(vec!["fs:read:/workspace/**".into()]),
        passthru_env: vec!["XAI_API_KEY".into()],
        mounts: vec![],
        workspace_mount: Some(WorkspaceMount {
            container_path: "/workspace".into(),
            readonly: false,
        }),
        sccache: true,
        extra_bootstrap: vec!["rustup component add rustfmt".into()],
        tool_packs: vec![],
    }
}

fn pack(
    name: &str,
    version: u32,
    binaries: Vec<String>,
    container_bootstrap: Vec<String>,
    allowed_tools_added: Vec<String>,
    system_prompt_fragment: Option<String>,
) -> ToolPack {
    ToolPack {
        name: name.into(),
        version,
        binaries,
        container_bootstrap,
        allowed_tools_added,
        system_prompt_fragment,
    }
}

#[test]
fn merge_with_no_packs_returns_clone() {
    let r = role(
        vec!["git".into()],
        Some(AllowedTools(vec!["Read".into()])),
        Some("base prompt".into()),
        "echo body",
    );
    let merged = merge_role_with_packs(&r, &[]);
    assert_eq!(
        merged, r,
        "empty packs slice must produce a structural clone"
    );
}

#[test]
fn merge_appends_binaries_dedup() {
    let r = role(vec!["git".into()], None, None, "echo body");
    let p1 = pack(
        "rust-cargo",
        1,
        vec!["cargo".into(), "git".into()],
        vec![],
        vec![],
        None,
    );
    let p2 = pack("rust-extra", 1, vec!["rustup".into()], vec![], vec![], None);
    let merged = merge_role_with_packs(&r, &[p1, p2]);
    assert_eq!(
        merged.binaries,
        vec!["git".to_string(), "cargo".into(), "rustup".into()],
        "binaries must dedup by string equality, preserving first-occurrence order",
    );
}

#[test]
fn merge_concatenates_system_prompt() {
    let r = role(vec![], None, Some("base".into()), "echo body");
    let p = pack("p", 1, vec![], vec![], vec![], Some("frag".into()));
    let merged = merge_role_with_packs(&r, &[p]);
    assert_eq!(
        merged.system_prompt.as_deref(),
        Some("base\n\nfrag"),
        "system_prompt fragments must join with two newlines",
    );
}

#[test]
fn merge_handles_none_system_prompt() {
    let r = role(vec![], None, None, "echo body");
    let p = pack("p", 1, vec![], vec![], vec![], Some("frag".into()));
    let merged = merge_role_with_packs(&r, &[p]);
    assert_eq!(
        merged.system_prompt.as_deref(),
        Some("frag"),
        "None+Some(fragment) must become Some(fragment)",
    );

    let r2 = role(vec![], None, None, "echo body");
    let p2 = pack("p", 1, vec![], vec![], vec![], None);
    let merged2 = merge_role_with_packs(&r2, &[p2]);
    assert!(
        merged2.system_prompt.is_none(),
        "None+None must stay None (no fragment to append)",
    );

    let r3 = role(vec![], None, Some("base".into()), "echo body");
    let p3 = pack("p", 1, vec![], vec![], vec![], None);
    let merged3 = merge_role_with_packs(&r3, &[p3]);
    assert_eq!(
        merged3.system_prompt.as_deref(),
        Some("base"),
        "Some(prompt)+None must keep Some(prompt) unchanged",
    );
}

#[test]
fn merge_prepends_entrypoint_bootstrap() {
    let r = role(vec![], None, None, "echo body");
    let p = pack(
        "p",
        1,
        vec![],
        vec!["set -e".into(), "rustup install stable".into()],
        vec![],
        None,
    );
    let merged = merge_role_with_packs(&r, &[p]);
    assert_eq!(
        merged.entrypoint_script, "set -e\nrustup install stable\necho body",
        "container_bootstrap lines must be prepended in order, joined by \\n, then a \\n separator before the role's own script body",
    );
}

#[test]
fn merge_appends_allowed_tools() {
    // Some(role)+pack: append.
    let r = role(
        vec![],
        Some(AllowedTools(vec!["Read".into()])),
        None,
        "echo body",
    );
    let p = pack("p", 1, vec![], vec![], vec!["Bash(cargo:*)".into()], None);
    let merged = merge_role_with_packs(&r, &[p]);
    assert_eq!(
        merged.allowed_tools.as_ref().map(|a| a.0.as_slice()),
        Some(&["Read".to_string(), "Bash(cargo:*)".into()][..]),
        "pack's allowed_tools_added must be appended to role's allowed_tools",
    );

    // None+pack with contributions: pack's tools become the role's allowed_tools.
    let r2 = role(vec![], None, None, "echo body");
    let p2 = pack(
        "p",
        1,
        vec![],
        vec![],
        vec!["Bash(cargo:*)".into(), "Read".into()],
        None,
    );
    let merged2 = merge_role_with_packs(&r2, &[p2]);
    assert_eq!(
        merged2.allowed_tools.as_ref().map(|a| a.0.as_slice()),
        Some(&["Bash(cargo:*)".to_string(), "Read".into()][..]),
        "None+pack-contributions must wrap pack tools in Some(AllowedTools(..))",
    );
}

#[test]
fn merge_multiple_packs_in_order() {
    let r = role(
        vec!["git".into()],
        Some(AllowedTools(vec!["Read".into()])),
        Some("base".into()),
        "echo body",
    );
    let p1 = pack(
        "p1",
        1,
        vec!["cargo".into()],
        vec!["p1-line-a".into(), "p1-line-b".into()],
        vec!["Bash(p1:*)".into()],
        Some("p1-frag".into()),
    );
    let p2 = pack(
        "p2",
        1,
        vec!["rustup".into()],
        vec!["p2-line".into()],
        vec!["Bash(p2:*)".into()],
        Some("p2-frag".into()),
    );
    let merged = merge_role_with_packs(&r, &[p1, p2]);

    assert_eq!(
        merged.binaries,
        vec!["git".to_string(), "cargo".into(), "rustup".into()],
        "binaries: role first, then p1 contributions, then p2 contributions",
    );
    assert_eq!(
        merged.allowed_tools.as_ref().map(|a| a.0.as_slice()),
        Some(&["Read".to_string(), "Bash(p1:*)".into(), "Bash(p2:*)".into()][..]),
        "allowed_tools: role first, then p1, then p2",
    );
    assert_eq!(
        merged.system_prompt.as_deref(),
        Some("base\n\np1-frag\n\np2-frag"),
        "system_prompt: role base, then p1 fragment, then p2 fragment",
    );
    assert_eq!(
        merged.entrypoint_script, "p1-line-a\np1-line-b\np2-line\necho body",
        "entrypoint: p1 lines (in order) then p2 lines, then a \\n, then role body",
    );
}
