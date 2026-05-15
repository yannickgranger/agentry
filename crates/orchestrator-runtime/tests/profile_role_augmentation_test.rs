//! Pure tests for `spawner::augment_role_with_profile` (slice I/2c).
//!
//! Exercises the augmentation rule that composes a brief's resolved
//! `.agentry/profile.toml` with the role's hardcoded `tool_packs` at
//! spawn time. The helper is pure (no Redis, no spawner state), so we
//! call it directly with synthesised `Profile` and `AgentRole` values
//! rather than constructing a full `RunAgentCtx`.

use orchestrator_runtime::spawner::augment_role_with_profile;
use orchestrator_types::{
    AgentRole, PackageManager, PermitScope, Profile, ProfileRoleSection, RoleName, SubstrateClass,
    ToolAllowlist,
};
use std::borrow::Cow;

fn role_with(name: &str, tool_packs: Vec<String>) -> AgentRole {
    AgentRole {
        name: RoleName(name.into()),
        version: 1,
        model: None,
        system_prompt: None,
        image: "alpine:3.21".into(),
        substrate_class: SubstrateClass::Podman,
        package_manager: PackageManager::Apk,
        entrypoint_script: "echo body".into(),
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
        tool_packs,
    }
}

fn profile_with(coder_packs: Vec<String>, reviewer_packs: Vec<String>) -> Profile {
    Profile {
        coder: ProfileRoleSection {
            tool_packs: coder_packs,
        },
        reviewer: ProfileRoleSection {
            tool_packs: reviewer_packs,
        },
        ..Profile::default()
    }
}

#[test]
fn coder_role_augmented_when_profile_has_coder_packs() {
    let role = role_with("coder-claude-agentry", vec!["quality-fast".into()]);
    let profile = profile_with(vec!["cfdb-grounding".into()], vec![]);

    let augmented = augment_role_with_profile(&role, Some("coder"), Some(&profile));

    assert!(matches!(augmented, Cow::Owned(_)));
    assert_eq!(
        augmented.tool_packs,
        vec!["quality-fast".to_string(), "cfdb-grounding".to_string()]
    );
}

#[test]
fn reviewer_role_augmented_when_profile_has_reviewer_packs() {
    let role = role_with("reviewer-claude-agentry", vec!["quality-fast".into()]);
    let profile = profile_with(vec![], vec!["audit-split-brain".into()]);

    let augmented = augment_role_with_profile(&role, Some("reviewer"), Some(&profile));

    assert!(matches!(augmented, Cow::Owned(_)));
    assert_eq!(
        augmented.tool_packs,
        vec!["quality-fast".to_string(), "audit-split-brain".to_string()]
    );
}

#[test]
fn non_coder_non_reviewer_not_augmented() {
    let role = role_with("shipper-agentry", vec!["quality-fast".into()]);
    let profile = profile_with(
        vec!["cfdb-grounding".into()],
        vec!["audit-split-brain".into()],
    );

    let augmented = augment_role_with_profile(&role, Some("shipper"), Some(&profile));

    assert!(matches!(augmented, Cow::Borrowed(_)));
    assert_eq!(augmented.tool_packs, vec!["quality-fast".to_string()]);
}

#[test]
fn no_profile_means_no_augmentation() {
    let role = role_with("coder-claude-agentry", vec!["quality-fast".into()]);

    let augmented = augment_role_with_profile(&role, Some("coder"), None);

    assert!(matches!(augmented, Cow::Borrowed(_)));
    assert_eq!(augmented.tool_packs, vec!["quality-fast".to_string()]);
}

#[test]
fn empty_profile_packs_means_no_augmentation() {
    let role = role_with("coder-claude-agentry", vec!["quality-fast".into()]);
    let profile = profile_with(vec![], vec![]);

    let augmented_coder = augment_role_with_profile(&role, Some("coder"), Some(&profile));
    assert!(matches!(augmented_coder, Cow::Borrowed(_)));
    assert_eq!(augmented_coder.tool_packs, vec!["quality-fast".to_string()]);

    let reviewer_role = role_with("reviewer-claude-agentry", vec!["quality-fast".into()]);
    let augmented_reviewer =
        augment_role_with_profile(&reviewer_role, Some("reviewer"), Some(&profile));
    assert!(matches!(augmented_reviewer, Cow::Borrowed(_)));
    assert_eq!(
        augmented_reviewer.tool_packs,
        vec!["quality-fast".to_string()]
    );
}

#[test]
fn profile_pack_already_in_role_not_duplicated() {
    let role = role_with("coder-claude-agentry", vec!["quality-fast".into()]);
    let profile = profile_with(vec!["quality-fast".into(), "cfdb-grounding".into()], vec![]);

    let augmented = augment_role_with_profile(&role, Some("coder"), Some(&profile));

    assert_eq!(
        augmented.tool_packs,
        vec!["quality-fast".to_string(), "cfdb-grounding".to_string()],
        "duplicates must be deduped, not double-listed"
    );
}
