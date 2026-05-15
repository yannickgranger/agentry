//! Tests for `anchor_resolver` — spec-concept resolution, cfdb qname-guard
//! and NotFound categorization, and the variant dispatcher.
//!
//! No live cfdb shell-out: the runner is not guaranteed to have `cfdb` on
//! PATH, and B6 acceptance exercises live cfdb via `scripts/arch-check.sh`.

use orchestrator_runtime::anchor_resolver::{
    resolve_assertion, resolve_cfdb_anchor, resolve_spec_concept_anchor, AnchorResolution,
    ResolverContext,
};
use orchestrator_types::contract::AssertionAnchor;
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

fn ctx_for_cfdb(cfdb_db: PathBuf) -> ResolverContext {
    ResolverContext {
        cfdb_db,
        cfdb_keyspace: "agentry".to_string(),
        specs_dir: PathBuf::from("/nonexistent/specs-for-cfdb-tests"),
    }
}

const SPEC_BODY: &str = "# FooHeading\n\nbody\n## SubHeading\n";

// ---- spec-concept resolver tests --------------------------------------

#[test]
fn resolve_spec_concept_anchor_resolves_when_heading_present_exact() {
    let dir = tempdir().expect("tmp");
    write_spec(dir.path(), "foo.md", SPEC_BODY);
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let res = resolve_spec_concept_anchor(&PathBuf::from("foo.md"), "SubHeading", &ctx);
    assert!(
        matches!(res, AnchorResolution::Resolved),
        "expected Resolved for present heading"
    );
}

#[test]
fn resolve_spec_concept_anchor_resolves_when_heading_present_case_insensitive() {
    let dir = tempdir().expect("tmp");
    write_spec(dir.path(), "foo.md", SPEC_BODY);
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let res = resolve_spec_concept_anchor(&PathBuf::from("foo.md"), "subheading", &ctx);
    assert!(
        matches!(res, AnchorResolution::Resolved),
        "expected Resolved for case-mismatched heading"
    );
}

#[test]
fn resolve_spec_concept_anchor_not_found_when_section_absent() {
    let dir = tempdir().expect("tmp");
    write_spec(dir.path(), "foo.md", SPEC_BODY);
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let res = resolve_spec_concept_anchor(&PathBuf::from("foo.md"), "MissingHeading", &ctx);
    match res {
        AnchorResolution::NotFound { reason } => assert!(
            reason.contains("no heading matching"),
            "reason should mention missing heading, got: {reason}"
        ),
        _ => panic!("expected NotFound for absent section"),
    }
}

#[test]
fn resolve_spec_concept_anchor_not_found_when_file_missing() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let res = resolve_spec_concept_anchor(&PathBuf::from("nonexistent.md"), "Anything", &ctx);
    match res {
        AnchorResolution::NotFound { reason } => assert!(
            reason.contains("not readable"),
            "reason should mention unreadable file, got: {reason}"
        ),
        _ => panic!("expected NotFound for missing file"),
    }
}

#[test]
fn resolve_spec_concept_anchor_rejects_absolute_path() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let res = resolve_spec_concept_anchor(&PathBuf::from("/etc/passwd"), "anything", &ctx);
    match res {
        AnchorResolution::NotFound { reason } => assert!(
            reason.contains("must be relative"),
            "reason should mention relative-path rule, got: {reason}"
        ),
        _ => panic!("expected NotFound for absolute path"),
    }
}

#[test]
fn resolve_spec_concept_anchor_rejects_parent_traversal() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let res = resolve_spec_concept_anchor(&PathBuf::from("../etc/passwd"), "anything", &ctx);
    match res {
        AnchorResolution::NotFound { reason } => assert!(
            reason.contains("no parent components"),
            "reason should mention parent-component rule, got: {reason}"
        ),
        _ => panic!("expected NotFound for parent traversal"),
    }
}

// ---- cfdb resolver tests ----------------------------------------------

#[test]
fn resolve_cfdb_anchor_rejects_qname_with_double_quote() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_cfdb(dir.path().join("does-not-exist"));
    let res = resolve_cfdb_anchor("evil\"qname::Foo", &ctx);
    match res {
        AnchorResolution::NotFound { reason } => assert!(
            reason.contains("illegal character"),
            "reason should mention the injection guard, got: {reason}"
        ),
        _ => panic!("expected NotFound for qname containing double quote"),
    }
}

#[test]
fn resolve_cfdb_anchor_returns_not_found_when_db_path_does_not_exist() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_cfdb(dir.path().join("nonexistent-db-dir"));
    let res = resolve_cfdb_anchor("some::valid::Qname", &ctx);
    // Reason may legitimately come from Case A (spawn failed — no cfdb on
    // PATH), Case C (empty stdout), or another documented NotFound path.
    // We assert NotFound only — not an exact reason string.
    assert!(
        matches!(res, AnchorResolution::NotFound { .. }),
        "expected NotFound when cfdb_db points at a nonexistent directory"
    );
}

// ---- dispatcher tests -------------------------------------------------

#[test]
fn resolve_assertion_dispatches_cfdb_variant_to_cfdb_resolver() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_cfdb(dir.path().join("nonexistent-db-dir"));
    let anchor = AssertionAnchor::Cfdb {
        qname: "some::valid::Qname".to_string(),
    };
    let res = resolve_assertion(&anchor, &ctx);
    assert!(
        matches!(res, AnchorResolution::NotFound { .. }),
        "dispatcher should route Cfdb to cfdb resolver, which then NotFounds"
    );
}

#[test]
fn resolve_assertion_dispatches_spec_concept_variant_to_spec_resolver() {
    let dir = tempdir().expect("tmp");
    write_spec(dir.path(), "foo.md", SPEC_BODY);
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let anchor = AssertionAnchor::SpecConcept {
        path: PathBuf::from("foo.md"),
        section: "SubHeading".to_string(),
    };
    let res = resolve_assertion(&anchor, &ctx);
    assert!(
        matches!(res, AnchorResolution::Resolved),
        "dispatcher should route SpecConcept to spec resolver, which then Resolves"
    );
}

#[test]
fn resolve_assertion_passes_through_behavior_variant() {
    let dir = tempdir().expect("tmp");
    let ctx = ctx_for_specs(dir.path().to_path_buf());
    let anchor = AssertionAnchor::Behavior {
        live_target: "any string".to_string(),
    };
    let res = resolve_assertion(&anchor, &ctx);
    assert!(
        matches!(res, AnchorResolution::Resolved),
        "Behavior is pass-through Resolved in B6"
    );
}
