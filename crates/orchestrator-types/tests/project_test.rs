use orchestrator_types::{EscalationMode, Project, ProjectSlug, StandingOrders, TeamName};

#[test]
fn project_roundtrip_json() {
    let p = Project {
        slug: ProjectSlug("qbot-core".into()),
        name: "qbot-core".into(),
        forges: vec!["agency:yg/qbot-core".into()],
        default_topology: Some(TeamName("qbot-issue-team".into())),
        steward_topology: Some(TeamName("qbot-steward".into())),
        standing_orders: StandingOrders {
            tokens_daily: Some(2_000_000),
            usd_daily: Some(20.0),
            default_escalation: EscalationMode::Autonomous,
            priorities: vec!["close RFC-023".into()],
            forbidden: vec!["git:force-push:main".into()],
        },
        repo_url: None,
        base_branch: None,
        max_concurrent_briefs: None,
    };
    let s = serde_json::to_string(&p).expect("ser");
    let back: Project = serde_json::from_str(&s).expect("de");
    assert_eq!(p, back);
}

#[test]
fn project_roundtrips_with_repo_url() {
    let p = Project {
        slug: ProjectSlug("agentry".into()),
        name: "agentry".into(),
        forges: vec!["agency:yg/agentry".into()],
        default_topology: Some(TeamName("agentry-self-host-v0".into())),
        steward_topology: None,
        standing_orders: StandingOrders::default(),
        repo_url: Some("https://agency.lab:3000/yg/agentry.git".into()),
        base_branch: Some("develop".into()),
        max_concurrent_briefs: None,
    };
    let s = serde_json::to_string(&p).expect("ser");
    let back: Project = serde_json::from_str(&s).expect("de");
    assert_eq!(p, back);
}

#[test]
fn project_roundtrips_with_max_concurrent_briefs() {
    let p = Project {
        slug: ProjectSlug("agentry".into()),
        name: "agentry".into(),
        forges: vec!["agency:yg/agentry".into()],
        default_topology: None,
        steward_topology: None,
        standing_orders: StandingOrders::default(),
        repo_url: None,
        base_branch: None,
        max_concurrent_briefs: Some(2),
    };
    let s = serde_json::to_string(&p).expect("ser");
    let back: Project = serde_json::from_str(&s).expect("de");
    assert_eq!(p, back);
}
