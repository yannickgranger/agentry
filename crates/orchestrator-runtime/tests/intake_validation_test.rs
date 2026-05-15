//! Tests for `intake_validation::validate_brief_contract` — exercises the
//! brief-level wrapper around the B6a anchor resolver against tempdir-backed
//! spec fixtures.
//!
//! Cfdb-anchor resolution is exercised at acceptance time via
//! `scripts/arch-check.sh`; these tests deliberately do not shell out to a
//! live cfdb. Daemon-level reject + spawn-skip integration is out of scope
//! for B6b — a future evidence brief observes the Failed verdict end-to-end.

use orchestrator_runtime::anchor_resolver::ResolverContext;
use orchestrator_runtime::intake_validation::validate_brief_contract;
use orchestrator_types::brief::{Brief, BriefId};
use orchestrator_types::contract::{Assertion, AssertionAnchor, AssertionId, Contract};
use orchestrator_types::{now, Budget, EscalationMode, TaskShape, VersionedRef};
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn write_spec(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write spec fixture");
    path
}

fn ctx_for_specs(specs_dir: PathBuf) -> ResolverContext {
    ResolverContext {
        cfdb_db: PathBuf::from("/nonexistent/cfdb-db-for-spec-tests"),
        cfdb_keyspace: "agentry".to_string(),
        specs_dir,
    }
}

fn build_brief(contract: Option<Contract>) -> Brief {
    Brief {
        id: BriefId("brf_test_intake_validation".into()),
        project: None,
        topology: VersionedRef::new("agentry-self-host-v0", 1),
        payload: serde_json::Value::Null,
        kind: Some(TaskShape::Feature),
        contract,
        budget: Budget::default(),
        escalation: EscalationMode::Autonomous,
        parent_brief: None,
        cohort_labels: vec![],
        redeploy_required: vec![],
        submitted_by: "test".into(),
        submitted_at: now(),
    }
}

const SPEC_BODY: &str = "# FooHeading\n\nbody\n## SubHeading\n";

#[test]
fn validate_returns_empty_when_brief_has_no_contract() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let brief = build_brief(None);
    let failures = validate_brief_contract(&brief, &ctx);
    assert!(
        failures.is_empty(),
        "no contract → no anchors to resolve → empty failures, got {failures:?}"
    );
}

#[test]
fn validate_returns_empty_when_all_spec_anchors_resolve() {
    let dir = tempdir().expect("tmp");
    write_spec(dir.path(), "foo.md", SPEC_BODY);
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let contract = Contract {
        brief_id: BriefId("brf_test_intake_validation".into()),
        assertions: vec![Assertion {
            id: AssertionId("A1".into()),
            prose: "FooHeading must exist".into(),
            anchor: AssertionAnchor::SpecConcept {
                path: PathBuf::from("foo.md"),
                section: "FooHeading".into(),
            },
        }],
        precursor_artifacts: vec![],
    };
    let brief = build_brief(Some(contract));
    let failures = validate_brief_contract(&brief, &ctx);
    assert!(
        failures.is_empty(),
        "all anchors resolve → empty failures, got {failures:?}"
    );
}

#[test]
fn validate_returns_failure_when_spec_section_missing() {
    let dir = tempdir().expect("tmp");
    write_spec(dir.path(), "foo.md", SPEC_BODY);
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let contract = Contract {
        brief_id: BriefId("brf_test_intake_validation".into()),
        assertions: vec![Assertion {
            id: AssertionId("A7".into()),
            prose: "MissingHeading must exist".into(),
            anchor: AssertionAnchor::SpecConcept {
                path: PathBuf::from("foo.md"),
                section: "MissingHeading".into(),
            },
        }],
        precursor_artifacts: vec![],
    };
    let brief = build_brief(Some(contract));
    let failures = validate_brief_contract(&brief, &ctx);
    assert_eq!(failures.len(), 1, "exactly one failure expected");
    assert_eq!(
        failures[0].0,
        AssertionId("A7".into()),
        "failure must carry requested AssertionId"
    );
    assert!(
        failures[0].1.contains("no heading matching"),
        "failure reason must surface the spec resolver's diagnostic, got: {}",
        failures[0].1
    );
}

#[test]
fn validate_returns_one_failure_per_unresolved_anchor() {
    let dir = tempdir().expect("tmp");
    write_spec(dir.path(), "foo.md", SPEC_BODY);
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let contract = Contract {
        brief_id: BriefId("brf_test_intake_validation".into()),
        assertions: vec![
            Assertion {
                id: AssertionId("A1".into()),
                prose: "FooHeading present".into(),
                anchor: AssertionAnchor::SpecConcept {
                    path: PathBuf::from("foo.md"),
                    section: "FooHeading".into(),
                },
            },
            Assertion {
                id: AssertionId("A2".into()),
                prose: "SubHeading present".into(),
                anchor: AssertionAnchor::SpecConcept {
                    path: PathBuf::from("foo.md"),
                    section: "SubHeading".into(),
                },
            },
            Assertion {
                id: AssertionId("A3".into()),
                prose: "GhostHeading absent".into(),
                anchor: AssertionAnchor::SpecConcept {
                    path: PathBuf::from("foo.md"),
                    section: "GhostHeading".into(),
                },
            },
        ],
        precursor_artifacts: vec![],
    };
    let brief = build_brief(Some(contract));
    let failures = validate_brief_contract(&brief, &ctx);
    assert_eq!(
        failures.len(),
        1,
        "two resolved + one unresolved → one failure"
    );
    assert_eq!(failures[0].0, AssertionId("A3".into()));
}

#[test]
fn validate_returns_multiple_failures_when_multiple_anchors_unresolved() {
    let dir = tempdir().expect("tmp");
    write_spec(dir.path(), "foo.md", SPEC_BODY);
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let contract = Contract {
        brief_id: BriefId("brf_test_intake_validation".into()),
        assertions: vec![
            Assertion {
                id: AssertionId("A1".into()),
                prose: "miss one".into(),
                anchor: AssertionAnchor::SpecConcept {
                    path: PathBuf::from("foo.md"),
                    section: "Miss1".into(),
                },
            },
            Assertion {
                id: AssertionId("A2".into()),
                prose: "miss two".into(),
                anchor: AssertionAnchor::SpecConcept {
                    path: PathBuf::from("foo.md"),
                    section: "Miss2".into(),
                },
            },
            Assertion {
                id: AssertionId("A3".into()),
                prose: "miss three".into(),
                anchor: AssertionAnchor::SpecConcept {
                    path: PathBuf::from("foo.md"),
                    section: "Miss3".into(),
                },
            },
        ],
        precursor_artifacts: vec![],
    };
    let brief = build_brief(Some(contract));
    let failures = validate_brief_contract(&brief, &ctx);
    assert_eq!(
        failures.len(),
        3,
        "three unresolved anchors → three failures"
    );
}

#[test]
fn validate_passes_through_behavior_anchor() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let contract = Contract {
        brief_id: BriefId("brf_test_intake_validation".into()),
        assertions: vec![Assertion {
            id: AssertionId("A1".into()),
            prose: "live target — pass-through in B6".into(),
            anchor: AssertionAnchor::Behavior {
                live_target: "any".into(),
            },
        }],
        precursor_artifacts: vec![],
    };
    let brief = build_brief(Some(contract));
    let failures = validate_brief_contract(&brief, &ctx);
    assert!(
        failures.is_empty(),
        "Behavior anchors are pass-through Resolved; expected no failures, got {failures:?}"
    );
}
