use orchestrator_types::{FindingOrigin, ReviewFinding, Severity};

#[test]
fn finding_roundtrip_json_mechanical() {
    let f = ReviewFinding {
        file: Some("crates/x/src/lib.rs".into()),
        line: Some(42),
        severity: Severity::Blocker,
        origin: FindingOrigin::Mechanical {
            tool: "clippy".into(),
            rule: Some("clippy::unwrap_used".into()),
        },
        category: "correctness".into(),
        message: "used `unwrap` on a `Result` value".into(),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    };
    let s = serde_json::to_string(&f).expect("ser");
    let back: ReviewFinding = serde_json::from_str(&s).expect("de");
    assert_eq!(f, back);
}

#[test]
fn finding_roundtrip_json_model() {
    let f = ReviewFinding {
        file: None,
        line: None,
        severity: Severity::Warn,
        origin: FindingOrigin::Model {
            reviewer_agent_id: "rev-claude-agentry:abc123".into(),
        },
        category: "design".into(),
        message: "consider splitting this function".into(),
        suggested_fix: Some("extract lines 40-55 into a helper".into()),
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    };
    let s = serde_json::to_string(&f).expect("ser");
    let back: ReviewFinding = serde_json::from_str(&s).expect("de");
    assert_eq!(f, back);
}

#[test]
fn finding_roundtrip_with_prohibitions_and_requirements() {
    let f = ReviewFinding {
        file: Some("crates/y/src/lib.rs".into()),
        line: Some(7),
        severity: Severity::Blocker,
        origin: FindingOrigin::Model {
            reviewer_agent_id: "rev-claude-agentry:def456".into(),
        },
        category: "design".into(),
        message: "rework needed: invariant violation".into(),
        suggested_fix: None,
        prohibitions: vec![
            "do not introduce a new abstraction".into(),
            "do not modify files outside the diff scope".into(),
        ],
        requirements: vec![
            "preserve the existing AgentRole permit_scope minimality".into(),
            "the marker line must be the LAST non-empty line".into(),
        ],
    };
    let s = serde_json::to_string(&f).expect("ser");
    let back: ReviewFinding = serde_json::from_str(&s).expect("de");
    assert_eq!(f, back);
    assert_eq!(back.prohibitions.len(), 2);
    assert_eq!(back.requirements.len(), 2);
}

#[test]
fn finding_deserializes_legacy_json_without_new_fields() {
    // Old emitter (before prohibitions/requirements existed) — must still
    // deserialize, with both new fields defaulting to empty Vec.
    let legacy = r#"{
        "file": "crates/z/src/lib.rs",
        "line": 1,
        "severity": "blocker",
        "origin": {"kind": "mechanical", "tool": "clippy", "rule": null},
        "category": "lint",
        "message": "old finding",
        "suggested_fix": null
    }"#;
    let f: ReviewFinding = serde_json::from_str(legacy).expect("de legacy");
    assert!(f.prohibitions.is_empty());
    assert!(f.requirements.is_empty());
}

#[test]
fn severity_serializes_snake_case() {
    let s = serde_json::to_string(&Severity::Blocker).expect("ser");
    assert_eq!(s, "\"blocker\"");
    let s = serde_json::to_string(&Severity::Warn).expect("ser");
    assert_eq!(s, "\"warn\"");
}

#[test]
fn origin_mechanical_tagged() {
    let o = FindingOrigin::Mechanical {
        tool: "cargo".into(),
        rule: None,
    };
    let s = serde_json::to_string(&o).expect("ser");
    assert!(s.contains("\"kind\":\"mechanical\""));
    assert!(s.contains("\"tool\":\"cargo\""));
}
