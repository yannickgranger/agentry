//! Tests for the ci-watcher-runner pure helpers (EPIC #161 Wave 2). The
//! workspace's `arch-ban-inline-cfg-test-in-src.cypher` rule (PR #295)
//! forbids inline `#[cfg(test)] mod tests` blocks in `src/`, so this is
//! a separate test-crate file (mirrors `coder_helpers_test.rs`).

use agentry_role_runtime::ci_watcher_runner::{
    find_shipper_message, first_failing_context, rand_jitter, run_merge_retry_loop, AttemptResult,
    MergeRetryOutcome,
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

// ---------- merge-retry outcome tests ----------
//
// These exercise the pure decision shape `merge_with_retry` is built on:
// the binary maps `MergeRetryOutcome::ExhaustedTransient` to the
// chain-trigger path (Shipped verdict + pr_rebaser brief), and
// `NonTransientFail` to the Failed verdict — the asymmetry that keeps
// real merge errors loud while routing the dominant 405/409 race to
// pr-rebaser-agentry instead of leaving the workspace stranded.

#[test]
fn merge_retry_persistent_405_chain_triggers_rebaser() {
    let outcome = run_merge_retry_loop(
        3,
        |_attempt| AttemptResult::Ok("405".into(), "merge conflict".into()),
        |_, _, _| {},
    );
    match outcome {
        MergeRetryOutcome::ExhaustedTransient { code, detail } => {
            assert_eq!(code, "405");
            assert_eq!(detail, "merge conflict");
        }
        other => panic!("expected ExhaustedTransient on persistent 405, got {other:?}"),
    }
}

#[test]
fn merge_retry_persistent_409_chain_triggers_rebaser() {
    let outcome = run_merge_retry_loop(
        2,
        |_attempt| AttemptResult::Ok("409".into(), "develop advanced".into()),
        |_, _, _| {},
    );
    assert!(matches!(
        outcome,
        MergeRetryOutcome::ExhaustedTransient { .. }
    ));
}

#[test]
fn merge_retry_first_attempt_200_ships() {
    let outcome = run_merge_retry_loop(
        6,
        |_attempt| AttemptResult::Ok("200".into(), String::new()),
        |_, _, _| panic!("on_transient must not fire on a 200"),
    );
    assert_eq!(outcome, MergeRetryOutcome::Merged { attempt: 1 });
}

#[test]
fn merge_retry_204_ships() {
    let outcome = run_merge_retry_loop(
        6,
        |_attempt| AttemptResult::Ok("204".into(), String::new()),
        |_, _, _| {},
    );
    assert_eq!(outcome, MergeRetryOutcome::Merged { attempt: 1 });
}

#[test]
fn merge_retry_405_then_200_ships_with_correct_attempt() {
    let mut transient_calls = 0u32;
    let outcome = run_merge_retry_loop(
        4,
        |attempt| {
            if attempt < 3 {
                AttemptResult::Ok("405".into(), "race".into())
            } else {
                AttemptResult::Ok("200".into(), String::new())
            }
        },
        |_, _, _| transient_calls += 1,
    );
    assert_eq!(outcome, MergeRetryOutcome::Merged { attempt: 3 });
    assert_eq!(transient_calls, 2);
}

#[test]
fn merge_retry_non_transient_500_emits_failed() {
    let outcome = run_merge_retry_loop(
        6,
        |_attempt| AttemptResult::Ok("500".into(), "internal server error".into()),
        |_, _, _| panic!("on_transient must not fire on a non-transient code"),
    );
    match outcome {
        MergeRetryOutcome::NonTransientFail {
            code,
            detail,
            attempt,
        } => {
            assert_eq!(code, "500");
            assert_eq!(detail, "internal server error");
            assert_eq!(attempt, 1);
        }
        other => panic!("expected NonTransientFail on 500, got {other:?}"),
    }
}

#[test]
fn merge_retry_persistent_spawn_error_chain_triggers() {
    let outcome = run_merge_retry_loop(
        2,
        |_attempt| AttemptResult::Err("curl: connection refused".into()),
        |_, _, _| {},
    );
    match outcome {
        MergeRetryOutcome::ExhaustedTransient { code, detail } => {
            assert_eq!(code, "(spawn-error)");
            assert_eq!(detail, "curl: connection refused");
        }
        other => panic!("expected ExhaustedTransient on persistent spawn error, got {other:?}"),
    }
}
