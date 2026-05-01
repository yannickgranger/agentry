use orchestrator_types::{
    now, AllowedTools, BriefId, PermitScope, RoleName, ToolAllowlist, WorkPermit,
};

#[test]
fn permit_roundtrip_json() {
    let p = WorkPermit {
        permit_id: "prm_test".into(),
        agent_id: "agt_1".into(),
        role: RoleName("coder-rust".into()),
        brief: BriefId("brf_1".into()),
        tool_allowlist: ToolAllowlist(vec!["read".into(), "edit".into()]),
        allowed_tools: None,
        permit_scope: PermitScope(vec!["fs:read:/workspace/**".into()]),
        max_tokens: Some(500_000),
        max_wall_seconds: Some(3600),
        max_usd: Some(5.0),
        expires_at: now() + chrono::Duration::hours(1),
        issued_at: now(),
        signature: None,
    };
    let s = serde_json::to_string(&p).expect("ser");
    let back: WorkPermit = serde_json::from_str(&s).expect("de");
    assert_eq!(p, back);
}

#[test]
fn permit_allows_checks_allowlist() {
    let p = WorkPermit {
        permit_id: "x".into(),
        agent_id: "x".into(),
        role: RoleName("x".into()),
        brief: BriefId("x".into()),
        tool_allowlist: ToolAllowlist(vec!["read".into()]),
        allowed_tools: None,
        permit_scope: PermitScope::default(),
        max_tokens: None,
        max_wall_seconds: None,
        max_usd: None,
        expires_at: now(),
        issued_at: now(),
        signature: None,
    };
    assert!(p.allows("read"));
    assert!(!p.allows("write"));
}

#[test]
fn permit_roundtrip_with_allowed_tools_some() {
    let p = WorkPermit {
        permit_id: "prm_t".into(),
        agent_id: "agt_t".into(),
        role: RoleName("coder".into()),
        brief: BriefId("brf_t".into()),
        tool_allowlist: ToolAllowlist(vec!["read".into()]),
        allowed_tools: Some(AllowedTools(vec!["Read".into(), "Bash(*)".into()])),
        permit_scope: PermitScope::default(),
        max_tokens: None,
        max_wall_seconds: None,
        max_usd: None,
        expires_at: now() + chrono::Duration::hours(1),
        issued_at: now(),
        signature: None,
    };
    let s = serde_json::to_string(&p).expect("ser");
    let back: WorkPermit = serde_json::from_str(&s).expect("de");
    assert_eq!(p, back);
    assert_eq!(back.allowed_tools.as_ref().map(|a| a.0.len()), Some(2));
}

#[test]
fn permit_roundtrip_with_allowed_tools_none_omits_field() {
    let p = WorkPermit {
        permit_id: "prm_t".into(),
        agent_id: "agt_t".into(),
        role: RoleName("coder".into()),
        brief: BriefId("brf_t".into()),
        tool_allowlist: ToolAllowlist::default(),
        allowed_tools: None,
        permit_scope: PermitScope::default(),
        max_tokens: None,
        max_wall_seconds: None,
        max_usd: None,
        expires_at: now() + chrono::Duration::hours(1),
        issued_at: now(),
        signature: None,
    };
    let s = serde_json::to_string(&p).expect("ser");
    assert!(!s.contains("allowed_tools"));
    let back: WorkPermit = serde_json::from_str(&s).expect("de");
    assert_eq!(p, back);
    assert!(back.allowed_tools.is_none());
}
