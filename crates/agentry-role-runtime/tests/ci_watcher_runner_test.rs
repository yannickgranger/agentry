//! Tests for the ci-watcher-runner pure helpers (EPIC #161 Wave 2). The
//! workspace's `arch-ban-inline-cfg-test-in-src.cypher` rule (PR #295)
//! forbids inline `#[cfg(test)] mod tests` blocks in `src/`, so this is
//! a separate test-crate file (mirrors `coder_helpers_test.rs`).

use agentry_role_runtime::ci_watcher_runner::{
    find_shipper_message, first_failing_context, rand_jitter,
};
use serde_json::json;

#[test]
fn find_shipper_message_returns_last_when_multiple() {
    let messages = [
        json!({"from": "coder-claude-agentry", "payload": {"x": 0}}),
        json!({"from": "shipper-agentry", "payload": {"pr_number": 1, "head_sha": "aaa"}}),
        json!({"from": "reviewer-claude-agentry", "payload": {"y": 0}}),
        json!({"from": "shipper-agentry", "payload": {"pr_number": 2, "head_sha": "bbb"}}),
    ];
    let last = find_shipper_message(&messages).expect("shipper message present");
    assert_eq!(last.pointer("/payload/pr_number"), Some(&json!(2)));
    assert_eq!(
        last.pointer("/payload/head_sha").and_then(|v| v.as_str()),
        Some("bbb"),
    );
}

#[test]
fn find_shipper_message_returns_none_when_absent() {
    let messages = [
        json!({"from": "coder-claude-agentry", "payload": {}}),
        json!({"from": "reviewer-claude-agentry", "payload": {}}),
    ];
    assert!(find_shipper_message(&messages).is_none());
}

#[test]
fn find_shipper_message_returns_none_on_empty() {
    let messages: Vec<serde_json::Value> = Vec::new();
    assert!(find_shipper_message(&messages).is_none());
}

#[test]
fn first_failing_context_returns_first_failure() {
    let statuses = [
        json!({"state": "success", "context": "fmt"}),
        json!({"state": "failure", "context": "test-suite"}),
        json!({"state": "error", "context": "build"}),
    ];
    assert_eq!(
        first_failing_context(&statuses),
        Some("test-suite".to_string()),
    );
}

#[test]
fn first_failing_context_returns_first_error_when_no_failure() {
    let statuses = [
        json!({"state": "success", "context": "fmt"}),
        json!({"state": "error", "context": "build"}),
    ];
    assert_eq!(first_failing_context(&statuses), Some("build".to_string()));
}

#[test]
fn first_failing_context_returns_none_on_empty() {
    let statuses: Vec<serde_json::Value> = Vec::new();
    assert!(first_failing_context(&statuses).is_none());
}

#[test]
fn first_failing_context_returns_none_when_all_green() {
    let statuses = [
        json!({"state": "success", "context": "fmt"}),
        json!({"state": "success", "context": "build"}),
    ];
    assert!(first_failing_context(&statuses).is_none());
}

#[test]
fn first_failing_context_returns_none_when_context_missing() {
    let statuses = [json!({"state": "failure"})];
    assert!(first_failing_context(&statuses).is_none());
}

#[test]
fn rand_jitter_in_range_0_9() {
    for _ in 0..100 {
        let j = rand_jitter();
        assert!(j <= 9, "jitter {j} outside 0..=9");
    }
}
