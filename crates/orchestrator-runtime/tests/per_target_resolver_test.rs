#![allow(clippy::expect_used, clippy::unwrap_used)]
use orchestrator_runtime::anchor_resolver::{sanitize_target_repo_slug, ResolverContext};
use orchestrator_runtime::intake_validation::{validate_brief_contract_for_target, IntakeError};
use orchestrator_types::brief::{Brief, BriefId};
use orchestrator_types::contract::{Assertion, AssertionAnchor, AssertionId, Contract};
use orchestrator_types::{Budget, EscalationMode, TaskShape, VersionedRef};
use serde_json::json;
use std::path::PathBuf;
use tempfile::tempdir;

fn build_brief_for_target(target_repo: &str, anchor: AssertionAnchor) -> Brief {
    Brief {
        id: BriefId("test".into()),
        project: None,
        topology: VersionedRef::new("agentry-self-host-v0", 1),
        payload: json!({"target_repo": target_repo}),
        kind: Some(TaskShape::Feature),
        contract: Some(Contract {
            brief_id: BriefId("test".into()),
            assertions: vec![Assertion {
                id: AssertionId("A1".into()),
                prose: "test".into(),
                anchor,
            }],
            precursor_artifacts: vec![],
        }),
        budget: Budget::default(),
        escalation: EscalationMode::Autonomous,
        parent_brief: None,
        cohort_labels: vec![],
        redeploy_required: vec![],
        submitted_by: "test".into(),
        submitted_at: orchestrator_types::now(),
    }
}

#[test]
fn slug_replaces_slashes_and_punctuation() {
    assert_eq!(sanitize_target_repo_slug("yg/agentry"), "yg_agentry");
    assert_eq!(sanitize_target_repo_slug("yg/glean"), "yg_glean");
    assert_eq!(
        sanitize_target_repo_slug("owner/repo-name.special"),
        "owner_repo_name_special"
    );
}

#[test]
fn slug_returns_empty_sentinel_when_strict_validator_rejects() {
    // Brief 1b: the legacy permissive byte-map fallback is gone.
    // Inputs that fail TargetRepo::from_str produce the empty-string
    // sentinel; daemon callers treat it as a hard error.
    assert_eq!(sanitize_target_repo_slug(""), "");
    assert_eq!(
        sanitize_target_repo_slug("yg/agentry@evil.example.com/foo"),
        ""
    );
    assert_eq!(sanitize_target_repo_slug("not-a-valid-target"), "");
}

fn build_brief_with_payload(payload: serde_json::Value, anchor: AssertionAnchor) -> Brief {
    Brief {
        id: BriefId("test".into()),
        project: None,
        topology: VersionedRef::new("agentry-self-host-v0", 1),
        payload,
        kind: Some(TaskShape::Feature),
        contract: Some(Contract {
            brief_id: BriefId("test".into()),
            assertions: vec![Assertion {
                id: AssertionId("A1".into()),
                prose: "test".into(),
                anchor,
            }],
            precursor_artifacts: vec![],
        }),
        budget: Budget::default(),
        escalation: EscalationMode::Autonomous,
        parent_brief: None,
        cohort_labels: vec![],
        redeploy_required: vec![],
        submitted_by: "test".into(),
        submitted_at: orchestrator_types::now(),
    }
}

#[test]
fn validate_brief_contract_for_target_rejects_when_target_repo_absent() {
    // Probe 4: missing payload.target_repo → MissingTargetRepo,
    // no _unknown keyspace, no clone attempted.
    let workspace = tempdir().unwrap();
    let brief = build_brief_with_payload(
        json!({}),
        AssertionAnchor::SpecConcept {
            path: PathBuf::from("scan.md"),
            section: "Anything".into(),
        },
    );
    let err = validate_brief_contract_for_target(&brief, workspace.path())
        .expect_err("absent target_repo must be rejected");
    assert_eq!(err, IntakeError::MissingTargetRepo);
}

#[test]
fn validate_brief_contract_for_target_rejects_url_fragment_injection() {
    // Probe 6: target_repo "yg/agentry@evil.example.com/foo" must be
    // rejected by TargetRepo::from_str charset validation. The token
    // never reaches a clone attempt against evil.example.com.
    let workspace = tempdir().unwrap();
    let brief = build_brief_with_payload(
        json!({"target_repo": "yg/agentry@evil.example.com/foo"}),
        AssertionAnchor::SpecConcept {
            path: PathBuf::from("scan.md"),
            section: "Anything".into(),
        },
    );
    let err = validate_brief_contract_for_target(&brief, workspace.path())
        .expect_err("malformed target_repo must be rejected");
    assert_eq!(err, IntakeError::MissingTargetRepo);
}

#[test]
fn for_target_repo_uses_per_target_paths() {
    let workspace = tempdir().unwrap();
    let ctx = ResolverContext::for_target_repo("yg/glean", workspace.path());
    assert!(ctx.cfdb_db.to_string_lossy().contains("/yg_glean"));
    assert_eq!(ctx.cfdb_keyspace, "yg_glean");
    assert_eq!(ctx.specs_dir, workspace.path().join("specs/concepts"));
}

#[test]
fn validate_brief_contract_for_target_resolves_spec_concept_against_brief_workspace() {
    let workspace = tempdir().unwrap();
    let specs_dir = workspace.path().join("specs/concepts");
    std::fs::create_dir_all(&specs_dir).unwrap();
    std::fs::write(specs_dir.join("scan.md"), "# Scan\n## SizeCatalog\nbody\n").unwrap();

    let brief = build_brief_for_target(
        "yg/glean",
        AssertionAnchor::SpecConcept {
            path: PathBuf::from("scan.md"),
            section: "SizeCatalog".into(),
        },
    );

    let failures = validate_brief_contract_for_target(&brief, workspace.path())
        .expect("brief target_repo parses");
    assert!(
        failures.is_empty(),
        "expected resolution against per-brief workspace, got: {failures:?}"
    );
}

#[test]
fn validate_brief_contract_for_target_returns_failure_when_section_absent() {
    let workspace = tempdir().unwrap();
    let specs_dir = workspace.path().join("specs/concepts");
    std::fs::create_dir_all(&specs_dir).unwrap();
    std::fs::write(specs_dir.join("scan.md"), "# Scan\n## OtherSection\n").unwrap();

    let brief = build_brief_for_target(
        "yg/glean",
        AssertionAnchor::SpecConcept {
            path: PathBuf::from("scan.md"),
            section: "MissingSection".into(),
        },
    );

    let failures = validate_brief_contract_for_target(&brief, workspace.path())
        .expect("brief target_repo parses");
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0].1.contains("no heading matching"),
        "got: {}",
        failures[0].1
    );
}
