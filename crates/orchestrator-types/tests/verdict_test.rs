use orchestrator_types::{BriefId, EventVerdict, Verdict, VerdictKind};

#[test]
fn verdict_roundtrip_json() {
    let v =
        Verdict::new(BriefId("brf_xyz".into()), VerdictKind::Shipped).with_reason("echo completed");
    let s = serde_json::to_string(&v).expect("ser");
    let back: Verdict = serde_json::from_str(&s).expect("de");
    assert_eq!(v, back);
    assert!(v.trace_stream.contains("brf_xyz"));
}

#[test]
fn event_verdict_maps() {
    assert_eq!(
        VerdictKind::from(EventVerdict::Shipped),
        VerdictKind::Shipped
    );
    assert_eq!(VerdictKind::from(EventVerdict::Failed), VerdictKind::Failed);
}

#[test]
fn rejected_roundtrips() {
    let v = Verdict::new(BriefId("brf_xyz".into()), VerdictKind::Rejected)
        .with_reason("fundamentally wrong approach");
    let s = serde_json::to_string(&v).expect("ser");
    let back: Verdict = serde_json::from_str(&s).expect("de");
    assert_eq!(v, back);
}

#[test]
fn refusal_count_roundtrips() {
    let mut v = Verdict::new(BriefId("brf_xyz".into()), VerdictKind::Failed);
    v.refusal_count = 5;
    let s = serde_json::to_string(&v).expect("ser");
    let back: Verdict = serde_json::from_str(&s).expect("de");
    assert_eq!(v, back);
    assert_eq!(back.refusal_count, 5);
}

#[test]
fn rework_needed_roundtrips() {
    use orchestrator_types::{FindingOrigin, ReviewFinding, Severity};
    let v = Verdict::new(
        BriefId("brf_xyz".into()),
        VerdictKind::ReworkNeeded {
            findings: vec![ReviewFinding {
                file: Some("src/lib.rs".into()),
                line: Some(10),
                severity: Severity::Blocker,
                origin: FindingOrigin::Mechanical {
                    tool: "clippy".into(),
                    rule: None,
                },
                category: "lint".into(),
                message: "example".into(),
                suggested_fix: None,
                prohibitions: Vec::new(),
                requirements: Vec::new(),
            }],
        },
    );
    let s = serde_json::to_string(&v).expect("ser");
    let back: Verdict = serde_json::from_str(&s).expect("de");
    assert_eq!(v, back);
}
