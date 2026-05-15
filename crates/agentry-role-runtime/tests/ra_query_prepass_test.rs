//! Tests for the ra-query pre-pass (Phase 2.2 #87).
//!
//! Covers the three contracts the brief calls out:
//!   - the JSON parsers (`prepass_unwraps_to_findings`,
//!     `prepass_complexity_to_findings`, `prepass_callers_to_findings`)
//!     produce Warn-severity, Mechanical-origin findings from typical
//!     ra-query JSON shapes;
//!   - the skip path (missing binary, non-git workspace) returns a
//!     [`RaQueryPrePass`] with `skipped_reason` populated and an empty
//!     finding vec, leaving the LLM-only review to continue;
//!   - the prompt assembly inlines `mechanical_findings_summary` under
//!     the dedicated section header so the LLM sees the file:line context.

use agentry_role_runtime::{
    build_review_prompt_with_mechanical_findings, format_mechanical_findings_summary,
    prepass_callers_to_findings, prepass_complexity_to_findings, prepass_unwraps_to_findings,
    ra_query_pre_pass,
};
use orchestrator_types::review::{FindingOrigin, Severity};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ---------- parsers ----------

#[test]
fn prepass_unwraps_typical_shape_emits_warns() {
    let j = json!({
        "functions": [
            { "name": "load", "line": 42, "total": 3 },
            { "name": "parse", "line": 99, "total": 1 },
        ],
    });
    let v = prepass_unwraps_to_findings("crates/foo/src/lib.rs", &j);
    assert_eq!(v.len(), 2);
    for f in &v {
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.category, "unwraps");
        assert_eq!(f.file.as_deref(), Some("crates/foo/src/lib.rs"));
        match &f.origin {
            FindingOrigin::Mechanical { tool, rule } => {
                assert_eq!(tool, "ra-query");
                assert_eq!(rule.as_deref(), Some("unwraps"));
            }
            other => panic!("expected Mechanical origin, got {other:?}"),
        }
    }
    assert_eq!(v[0].line, Some(42));
    assert!(v[0].message.contains("load"));
    assert!(v[0].message.contains('3'));
}

#[test]
fn prepass_complexity_typical_shape_emits_warns() {
    let j = json!({
        "functions": [
            { "name": "twisty", "line": 10, "cognitive": 22 },
        ],
    });
    let v = prepass_complexity_to_findings("crates/bar/src/m.rs", &j);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].severity, Severity::Warn);
    assert_eq!(v[0].category, "complexity");
    assert_eq!(v[0].line, Some(10));
    match &v[0].origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, "ra-query");
            assert_eq!(rule.as_deref(), Some("complexity"));
        }
        other => panic!("expected Mechanical origin, got {other:?}"),
    }
    assert!(v[0].message.contains("twisty"));
    assert!(v[0].message.contains("22"));
}

#[test]
fn prepass_callers_zero_emits_warn_finding() {
    let j = json!({"callers": []});
    let v = prepass_callers_to_findings("src/m.rs", "do_thing", 7, &j);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].severity, Severity::Warn);
    assert_eq!(v[0].category, "callers");
    assert_eq!(v[0].file.as_deref(), Some("src/m.rs"));
    assert_eq!(v[0].line, Some(7));
    match &v[0].origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, "ra-query");
            assert_eq!(rule.as_deref(), Some("callers_zero"));
        }
        other => panic!("expected Mechanical origin, got {other:?}"),
    }
    assert!(v[0].message.contains("do_thing"));
    assert!(v[0].message.contains("zero callers"));
}

#[test]
fn prepass_callers_with_hits_emits_nothing() {
    let j = json!({"callers": [{"file": "src/x.rs", "line": 1}]});
    assert!(prepass_callers_to_findings("src/m.rs", "do_thing", 7, &j).is_empty());
}

#[test]
fn prepass_callers_accepts_bare_array_shape() {
    let j = json!([]);
    let v = prepass_callers_to_findings("src/m.rs", "stub", 1, &j);
    assert_eq!(v.len(), 1);
    let j_hits = json!([{"file": "src/x.rs"}]);
    assert!(prepass_callers_to_findings("src/m.rs", "stub", 1, &j_hits).is_empty());
}

#[test]
fn prepass_parsers_handle_missing_or_empty_functions() {
    let empty = json!({"functions": []});
    assert!(prepass_unwraps_to_findings("x.rs", &empty).is_empty());
    assert!(prepass_complexity_to_findings("x.rs", &empty).is_empty());
    let no_field = json!({"summary": {}});
    assert!(prepass_unwraps_to_findings("x.rs", &no_field).is_empty());
    assert!(prepass_complexity_to_findings("x.rs", &no_field).is_empty());
}

// ---------- summary formatter ----------

#[test]
fn mechanical_findings_summary_empty_is_none_sentinel() {
    assert_eq!(format_mechanical_findings_summary(&[]), "(none)");
}

#[test]
fn mechanical_findings_summary_lines_match_severity_category_file_line_message() {
    let f = prepass_unwraps_to_findings(
        "crates/a/src/lib.rs",
        &json!({"functions": [{"name": "f", "line": 12, "total": 2}]}),
    );
    let s = format_mechanical_findings_summary(&f);
    // shape: severity:category:file:line:message
    let line = s.lines().next().expect("one line");
    assert!(line.starts_with("warn:unwraps:crates/a/src/lib.rs:12:"));
    assert!(line.contains("f"));
}

// ---------- prompt assembly ----------

#[test]
fn prompt_with_mechanical_findings_section_inlines_summary() {
    let summary = "warn:unwraps:src/foo.rs:42:foo: 3 high-severity unwrap/expect call(s)";
    let prompt = build_review_prompt_with_mechanical_findings(
        "develop",
        "Fix bug",
        "BODY",
        "DIFF_TEXT",
        summary,
    );
    assert!(prompt.contains("TITLE: Fix bug"));
    assert!(prompt.contains("DIFF_TEXT"));
    assert!(prompt.contains(
        "Mechanical findings already detected (do not re-flag, but consider them in your architectural review):"
    ));
    assert!(prompt.contains(summary));
}

#[test]
fn prompt_with_no_mechanical_findings_uses_none_sentinel_inline() {
    let prompt =
        build_review_prompt_with_mechanical_findings("develop", "T", "B", "DIFF", "(none)");
    let header = "Mechanical findings already detected";
    let idx = prompt.find(header).expect("section present");
    let tail = &prompt[idx..];
    assert!(tail.contains("(none)"));
}

// ---------- skip path ----------

// `std::env::set_var` is process-global, so the missing-binary test that
// stomps on `PATH` must serialize against any other test that touches the
// environment. A static mutex covers this — the skip test owns the env
// for its duration.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn skip_path_missing_binary_returns_skipped_reason_and_no_findings() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let original = std::env::var_os("PATH");
    // Point PATH at a directory ra-query is provably absent from. The
    // workspace tempdir doesn't matter here — `ra_query_present` runs
    // first and short-circuits before any git/file work.
    std::env::set_var("PATH", "/var/empty");
    let result = ra_query_pre_pass(Path::new("/workspace"), "develop");
    if let Some(p) = original {
        std::env::set_var("PATH", p);
    } else {
        std::env::remove_var("PATH");
    }
    assert!(result.findings.is_empty(), "skip path emits no findings");
    let reason = result
        .skipped_reason
        .as_deref()
        .expect("skip emits a reason");
    assert!(
        reason.contains("ra-query binary missing"),
        "missing-binary reason: got {reason:?}"
    );
}

#[test]
fn skip_path_non_git_workspace_returns_skipped_reason() {
    // Either ra-query is missing (skipped on first check) or the
    // workspace is not a git repo (skipped on git diff). Both paths
    // honor the same contract: skipped_reason set, findings empty.
    let dir = unique_tempdir("not-a-repo");
    let result = ra_query_pre_pass(&dir, "develop");
    assert!(result.findings.is_empty());
    assert!(
        result.skipped_reason.is_some(),
        "non-git workspace must emit a skip reason"
    );
}

fn unique_tempdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "agentry-prepass-test-{tag}-{}-{nanos}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}
