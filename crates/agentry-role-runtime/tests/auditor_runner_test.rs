//! Tests for the auditor-claude-runner ra-query helpers added in brief
//! #87 phase2. The runner binary itself spawns `ra-query` and `git`
//! subprocesses — both are integration-level concerns. These tests
//! cover the pure parser primitives exposed via the lib crate:
//!
//! - `parse_unwraps_findings` — ra-query unwraps JSON → Warn findings
//! - `parse_callers_count` — ra-query callers JSON → caller count
//! - `parse_diff_added_lines` — unified-zero diff → file → added lines
//! - `ra_query_skipped_event` — the skip-path event payload
//!
//! Per the inline-cfg-test ban
//! (`.cfdb/queries/arch-ban-inline-cfg-test-in-src.cypher`) these live
//! in `tests/`, not in `src/`.

use agentry_role_runtime::{
    orphan_pub_finding, parse_callers_count, parse_diff_added_lines, parse_unwraps_findings,
    ra_query_skipped_event, ORPHAN_PUB_CATEGORY, RA_QUERY_TOOL, UNWRAPS_CATEGORY,
};
use orchestrator_types::{FindingOrigin, Severity};
use serde_json::{json, Value};

// -- parse_unwraps_findings -------------------------------------------

#[test]
fn parse_unwraps_findings_empty_functions_returns_empty_vec() {
    let v: Value = json!({
        "file": "crates/foo/src/lib.rs",
        "functions": [],
        "summary": {"total": 0, "critical": 0, "in_tests": 0, "hotspot": null}
    });
    let findings = parse_unwraps_findings("crates/foo/src/lib.rs", &v);
    assert!(findings.is_empty());
}

#[test]
fn parse_unwraps_findings_single_unwrap_yields_warn_finding() {
    let v: Value = json!({
        "file": "crates/foo/src/lib.rs",
        "functions": [{
            "name": "fixed_times",
            "line": 10,
            "is_test": false,
            "unwraps": [{
                "line": 11,
                "column": 65,
                "method": "unwrap",
                "in_loop": false,
                "in_test": false,
                "after_ok_err": false,
                "severity": "medium"
            }],
            "total": 1,
            "critical": 0
        }]
    });
    let findings = parse_unwraps_findings("crates/foo/src/lib.rs", &v);
    assert_eq!(findings.len(), 1);
    let f = &findings[0];
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(f.category, UNWRAPS_CATEGORY);
    assert_eq!(f.file.as_deref(), Some("crates/foo/src/lib.rs"));
    assert_eq!(f.line, Some(11));
    assert!(
        f.message.contains("crates/foo/src/lib.rs:11:fixed_times"),
        "message must include file:line:fqn — got: {}",
        f.message
    );
    assert!(f.message.contains("unwrap"));
    assert!(f.message.contains("medium"));
    match &f.origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, RA_QUERY_TOOL);
            assert_eq!(rule.as_deref(), Some(UNWRAPS_CATEGORY));
        }
        FindingOrigin::Model { .. } => panic!("expected Mechanical origin"),
    }
}

#[test]
fn parse_unwraps_findings_multiple_functions_emit_one_per_unwrap() {
    let v: Value = json!({
        "file": "crates/foo/src/lib.rs",
        "functions": [
            {
                "name": "alpha",
                "line": 5,
                "is_test": false,
                "unwraps": [
                    {"line": 6, "column": 1, "method": "unwrap", "severity": "low"},
                    {"line": 7, "column": 2, "method": "expect", "severity": "high"}
                ],
                "total": 2,
                "critical": 0
            },
            {
                "name": "beta",
                "line": 20,
                "is_test": false,
                "unwraps": [
                    {"line": 22, "column": 3, "method": "unwrap", "severity": "critical"}
                ],
                "total": 1,
                "critical": 1
            }
        ]
    });
    let findings = parse_unwraps_findings("crates/foo/src/lib.rs", &v);
    assert_eq!(findings.len(), 3);
    // None should ever be Blocker — auditor stays informational.
    for f in &findings {
        assert_eq!(f.severity, Severity::Warn);
    }
    let lines: Vec<u32> = findings.iter().filter_map(|f| f.line).collect();
    assert_eq!(lines, vec![6, 7, 22]);
    assert!(findings[0].message.contains("alpha"));
    assert!(findings[1].message.contains("alpha"));
    assert!(findings[2].message.contains("beta"));
}

#[test]
fn parse_unwraps_findings_missing_functions_array_returns_empty() {
    // Defensive: malformed ra-query output (no `functions` field) must
    // not panic — caller treats as "clean file".
    let v: Value = json!({"file": "crates/foo/src/lib.rs"});
    let findings = parse_unwraps_findings("crates/foo/src/lib.rs", &v);
    assert!(findings.is_empty());
}

// -- parse_callers_count ----------------------------------------------

#[test]
fn parse_callers_count_empty_array_is_zero() {
    assert_eq!(parse_callers_count(&json!({"callers": []})), 0);
}

#[test]
fn parse_callers_count_counts_entries() {
    let v: Value = json!({
        "callers": [
            {"file": "crates/a/src/lib.rs", "line": 10},
            {"file": "crates/b/src/lib.rs", "line": 20},
            {"file": "crates/c/src/lib.rs", "line": 30}
        ]
    });
    assert_eq!(parse_callers_count(&v), 3);
}

#[test]
fn parse_callers_count_missing_field_is_zero() {
    // Mirrors existing pub-surface stage's `.callers | length` semantics:
    // an unrecognised shape reads as zero, never as an error.
    assert_eq!(parse_callers_count(&json!({})), 0);
    assert_eq!(parse_callers_count(&json!({"callers": "not-an-array"})), 0);
}

// -- ra_query_skipped_event -------------------------------------------

#[test]
fn ra_query_skipped_event_carries_stage_label_and_reason() {
    // The skip path: missing binary (or any best-effort failure) must
    // emit a single `event` with msg + reason — never a `finding`, and
    // never abort the auditor.
    let ev = ra_query_skipped_event("unwraps_findings", "ra-query binary missing");
    assert_eq!(
        ev.get("msg").and_then(Value::as_str),
        Some("ra-query unwraps_findings skipped")
    );
    assert_eq!(
        ev.get("reason").and_then(Value::as_str),
        Some("ra-query binary missing")
    );
}

#[test]
fn ra_query_skipped_event_works_for_orphan_pub_stage() {
    let ev = ra_query_skipped_event("orphan_pub", "git diff failed: bad rev");
    assert_eq!(
        ev.get("msg").and_then(Value::as_str),
        Some("ra-query orphan_pub skipped")
    );
    assert_eq!(
        ev.get("reason").and_then(Value::as_str),
        Some("git diff failed: bad rev")
    );
}

// -- parse_diff_added_lines -------------------------------------------

#[test]
fn parse_diff_added_lines_extracts_added_lines_per_file() {
    let diff = "\
diff --git a/crates/foo/src/lib.rs b/crates/foo/src/lib.rs
index 0000001..0000002 100644
--- a/crates/foo/src/lib.rs
+++ b/crates/foo/src/lib.rs
@@ -10,0 +11,2 @@
+pub fn new_one() {}
+pub fn new_two() {}
@@ -50,1 +52,0 @@
-old line
diff --git a/crates/bar/src/lib.rs b/crates/bar/src/lib.rs
--- a/crates/bar/src/lib.rs
+++ b/crates/bar/src/lib.rs
@@ -1,0 +2,1 @@
+pub struct New;
";
    let map = parse_diff_added_lines(diff);
    let foo = map
        .get("crates/foo/src/lib.rs")
        .expect("foo should be present");
    assert!(foo.contains(&11), "expected new line 11 in foo: {foo:?}");
    assert!(foo.contains(&12), "expected new line 12 in foo: {foo:?}");
    assert_eq!(foo.len(), 2);
    let bar = map
        .get("crates/bar/src/lib.rs")
        .expect("bar should be present");
    assert!(bar.contains(&2));
    assert_eq!(bar.len(), 1);
}

#[test]
fn parse_diff_added_lines_empty_diff_returns_empty_map() {
    assert!(parse_diff_added_lines("").is_empty());
}

// -- orphan_pub_finding ------------------------------------------------

#[test]
fn orphan_pub_finding_is_warn_with_canonical_message() {
    let f = orphan_pub_finding("crates/foo/src/lib.rs", 42, "do_thing", "fn");
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(f.category, ORPHAN_PUB_CATEGORY);
    assert_eq!(f.file.as_deref(), Some("crates/foo/src/lib.rs"));
    assert_eq!(f.line, Some(42));
    assert!(f.message.contains("crates/foo/src/lib.rs:42:do_thing"));
    assert!(f.message.contains("fn"));
    match &f.origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, RA_QUERY_TOOL);
            assert_eq!(rule.as_deref(), Some(ORPHAN_PUB_CATEGORY));
        }
        FindingOrigin::Model { .. } => panic!("expected Mechanical origin"),
    }
}
