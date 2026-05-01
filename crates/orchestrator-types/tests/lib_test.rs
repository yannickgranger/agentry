use orchestrator_types::{
    apply_overrides, check_tool_call, now, BriefId, PermitOverrides, PermitScope, RoleName,
    ToolAllowlist, WorkPermit,
};

fn sample_permit() -> WorkPermit {
    WorkPermit {
        permit_id: "x".into(),
        agent_id: "x".into(),
        role: RoleName("x".into()),
        brief: BriefId("x".into()),
        tool_allowlist: ToolAllowlist(vec!["read".into(), "write".into(), "edit".into()]),
        allowed_tools: None,
        permit_scope: PermitScope(vec![
            "fs:read:/workspace/**".into(),
            "fs:write:/workspace/**".into(),
        ]),
        max_tokens: None,
        max_wall_seconds: None,
        max_usd: None,
        expires_at: now(),
        issued_at: now(),
        signature: None,
    }
}

#[test]
fn fs_write_narrowing_replaces_wildcards() {
    let mut p = sample_permit();
    let o = PermitOverrides {
        fs_write: vec!["/workspace/a.rs".into()],
        ..Default::default()
    };
    apply_overrides(&mut p, &o);
    assert!(!p
        .permit_scope
        .0
        .iter()
        .any(|s| s == "fs:write:/workspace/**"));
    assert!(p
        .permit_scope
        .0
        .contains(&"fs:write:/workspace/a.rs".into()));
    // fs:read untouched.
    assert!(p.permit_scope.0.contains(&"fs:read:/workspace/**".into()));
}

#[test]
fn allowlist_override_intersects() {
    let mut p = sample_permit();
    let o = PermitOverrides {
        tool_allowlist: vec!["read".into(), "write".into()],
        ..Default::default()
    };
    apply_overrides(&mut p, &o);
    assert!(p.tool_allowlist.contains("read"));
    assert!(p.tool_allowlist.contains("write"));
    assert!(!p.tool_allowlist.contains("edit")); // dropped — not in override
}

#[test]
fn check_tool_call_allows_in_scope_write() {
    let mut p = sample_permit();
    apply_overrides(
        &mut p,
        &PermitOverrides {
            fs_write: vec!["/workspace/a.rs".into()],
            ..Default::default()
        },
    );
    let ok = check_tool_call(&p, "write", &serde_json::json!({"path":"/workspace/a.rs"}));
    assert!(ok.is_ok());
    let nope = check_tool_call(&p, "write", &serde_json::json!({"path":"/workspace/b.rs"}));
    assert!(nope.is_err());
}

#[test]
fn check_tool_call_blocks_unknown_tool() {
    let p = sample_permit();
    let nope = check_tool_call(&p, "shell", &serde_json::json!({}));
    assert!(nope.is_err());
}
