//! Tests for `run_fence` (brief Y.3) — smoke + threshold-folding.

use agentry_role_runtime::{
    clones_to_findings, complexity_to_findings, run_fence, unwraps_to_findings,
};
use orchestrator_types::review::{FindingOrigin, Severity};
use serde_json::json;
use std::path::Path;

#[test]
fn run_fence_pipeline_does_not_panic() {
    // Smoke: drives the real pipeline against /workspace. The test
    // environment may or may not have `origin/develop` available; both
    // outcomes (empty Vec, Vec with entries) are acceptable. The contract
    // we verify here is "returns Vec<ReviewFinding> without panicking".
    let v = run_fence(Path::new("/workspace"), "develop");
    let _len = v.len();
}

#[test]
fn clones_emits_in_loop_finding() {
    let json = json!({
        "functions": [{
            "name": "hot",
            "line": 42,
            "clone_calls": 1,
            "clones_in_loop": 1,
            "arc_rc_pattern": 0,
        }],
    });
    let v = clones_to_findings("src/foo.rs", &json);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].file.as_deref(), Some("src/foo.rs"));
    assert_eq!(v[0].line, Some(42));
    assert_eq!(v[0].severity, Severity::Blocker);
    match &v[0].origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, "ra-query");
            assert_eq!(rule.as_deref(), Some("clones_in_loop"));
        }
        other => panic!("expected Mechanical origin, got {other:?}"),
    }
}

#[test]
fn clones_emits_clone_prod_when_arc_rc_does_not_account_for_all() {
    let json = json!({
        "functions": [{
            "name": "noisy",
            "line": 10,
            "clone_calls": 3,
            "clones_in_loop": 0,
            "arc_rc_pattern": 1,
        }],
    });
    let v = clones_to_findings("src/foo.rs", &json);
    assert_eq!(v.len(), 1);
    match &v[0].origin {
        FindingOrigin::Mechanical { rule, .. } => {
            assert_eq!(rule.as_deref(), Some("clone_prod"));
        }
        other => panic!("expected Mechanical origin, got {other:?}"),
    }
}

#[test]
fn clones_emits_nothing_when_all_arc_rc() {
    let json = json!({
        "functions": [{
            "name": "ok",
            "line": 5,
            "clone_calls": 2,
            "clones_in_loop": 0,
            "arc_rc_pattern": 2,
        }],
    });
    assert!(clones_to_findings("src/foo.rs", &json).is_empty());
}

#[test]
fn complexity_emits_one_per_function() {
    let json = json!({
        "functions": [
            { "name": "a", "line": 1, "cognitive": 16 },
            { "name": "b", "line": 30, "cognitive": 25 },
        ],
    });
    let v = complexity_to_findings("src/foo.rs", &json);
    assert_eq!(v.len(), 2);
    for f in &v {
        assert_eq!(f.severity, Severity::Blocker);
        match &f.origin {
            FindingOrigin::Mechanical { rule, tool } => {
                assert_eq!(rule.as_deref(), Some("complexity"));
                assert_eq!(tool, "ra-query");
            }
            other => panic!("expected Mechanical origin, got {other:?}"),
        }
    }
}

#[test]
fn unwraps_emits_one_per_function_with_line() {
    let json = json!({
        "functions": [
            { "name": "f", "line": 8, "total": 2 },
        ],
    });
    let v = unwraps_to_findings("src/foo.rs", &json);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].line, Some(8));
    assert_eq!(v[0].file.as_deref(), Some("src/foo.rs"));
    match &v[0].origin {
        FindingOrigin::Mechanical { rule, .. } => {
            assert_eq!(rule.as_deref(), Some("unwraps"));
        }
        other => panic!("expected Mechanical origin, got {other:?}"),
    }
}

#[test]
fn empty_functions_yields_empty_findings() {
    let empty = json!({"functions": []});
    assert!(clones_to_findings("x.rs", &empty).is_empty());
    assert!(complexity_to_findings("x.rs", &empty).is_empty());
    assert!(unwraps_to_findings("x.rs", &empty).is_empty());
}

#[test]
fn missing_functions_field_is_handled() {
    let v = json!({"summary": {}});
    assert!(clones_to_findings("x.rs", &v).is_empty());
    assert!(complexity_to_findings("x.rs", &v).is_empty());
    assert!(unwraps_to_findings("x.rs", &v).is_empty());
}
