//! Unit tests for `captain freshness`' pure classification logic.
//!
//! These exercise `classify_against_content` — the line-count comparison
//! sub-helper carved out of `classify_ref` precisely so the comparison
//! path is testable without an HTTP mock. The HTTP-fronted `classify_ref`
//! itself is exercised end-to-end when an operator runs `captain
//! freshness` against a live issue.

use orchestrator_runtime::captain_freshness::{classify_against_content, RefStatus};

#[test]
fn out_of_range_when_expected_exceeds_actual() {
    let content = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10";
    let status = classify_against_content(content, Some(20));
    assert_eq!(status, RefStatus::OutOfRange { actual_lines: 10 });
}

#[test]
fn ok_when_no_line_expected() {
    let content = "1\n2\n3";
    let status = classify_against_content(content, None);
    assert_eq!(status, RefStatus::Ok);
}

#[test]
fn ok_when_line_within_range() {
    let content = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10";
    let status = classify_against_content(content, Some(5));
    assert_eq!(status, RefStatus::Ok);
}
