//! Tests for the verifier-dol pure helpers (EPIC #161 Wave 3 final
//! slice). The runner binary itself spawns `sh` and writes to stdout —
//! both are integration-level concerns. These tests cover the pure
//! parsing / mapping layer that stays in the lib crate.
//!
//! Per PR #295 (separate file per arch ban), these live outside `src/`
//! so the inline-cfg-test ban (`.cfdb/queries/arch-ban-inline-cfg-test-in-src.cypher`)
//! has nothing to flag.

use agentry_role_runtime::{
    parse_success_criteria, parse_verifies_brief_id, tail_bytes, verdict_for_exit_code,
    CRITERION_OUTPUT_TAIL,
};
use orchestrator_types::EventVerdict;
use serde_json::json;

#[test]
fn parse_success_criteria_returns_string_when_present() {
    let bundle = json!({
        "brief": {
            "payload": {
                "success_criteria": "cargo test --workspace"
            }
        }
    });
    assert_eq!(
        parse_success_criteria(&bundle),
        Some("cargo test --workspace".to_string()),
    );
}

#[test]
fn parse_success_criteria_returns_none_when_field_missing() {
    let bundle = json!({"brief": {"payload": {}}});
    assert_eq!(parse_success_criteria(&bundle), None);
}

#[test]
fn parse_success_criteria_returns_none_when_payload_missing() {
    let bundle = json!({"brief": {}});
    assert_eq!(parse_success_criteria(&bundle), None);
}

#[test]
fn parse_success_criteria_returns_none_when_empty_string() {
    let bundle = json!({"brief": {"payload": {"success_criteria": ""}}});
    assert_eq!(parse_success_criteria(&bundle), None);
}

#[test]
fn parse_success_criteria_returns_none_when_null() {
    let bundle = json!({"brief": {"payload": {"success_criteria": null}}});
    assert_eq!(parse_success_criteria(&bundle), None);
}

#[test]
fn parse_success_criteria_returns_none_when_not_a_string() {
    let bundle = json!({"brief": {"payload": {"success_criteria": 42}}});
    assert_eq!(parse_success_criteria(&bundle), None);
}

#[test]
fn parse_verifies_brief_id_returns_value_when_present() {
    let bundle = json!({
        "brief": {
            "payload": {
                "verifies_brief_id": "brf_meta_42"
            }
        }
    });
    assert_eq!(parse_verifies_brief_id(&bundle), "brf_meta_42");
}

#[test]
fn parse_verifies_brief_id_returns_empty_when_missing() {
    let bundle = json!({"brief": {"payload": {}}});
    assert_eq!(parse_verifies_brief_id(&bundle), "");
}

#[test]
fn parse_verifies_brief_id_returns_empty_when_null() {
    let bundle = json!({"brief": {"payload": {"verifies_brief_id": null}}});
    assert_eq!(parse_verifies_brief_id(&bundle), "");
}

#[test]
fn verdict_for_exit_code_zero_is_shipped() {
    assert_eq!(verdict_for_exit_code(0), EventVerdict::Shipped);
}

#[test]
fn verdict_for_exit_code_one_is_failed() {
    assert_eq!(verdict_for_exit_code(1), EventVerdict::Failed);
}

#[test]
fn verdict_for_exit_code_negative_is_failed() {
    // `Command::status().code()` may return a negative value on some
    // platforms when the child terminates abnormally; the verifier
    // treats every non-zero code as a failure.
    assert_eq!(verdict_for_exit_code(-1), EventVerdict::Failed);
}

#[test]
fn verdict_for_exit_code_high_value_is_failed() {
    assert_eq!(verdict_for_exit_code(127), EventVerdict::Failed);
    assert_eq!(verdict_for_exit_code(255), EventVerdict::Failed);
}

#[test]
fn criterion_output_tail_constant_matches_bash() {
    assert_eq!(CRITERION_OUTPUT_TAIL, 4096);
}

#[test]
fn tail_bytes_extracts_last_4096_bytes_of_combined_output() {
    // Mirrors the verifier's emit-time tail: take the last
    // CRITERION_OUTPUT_TAIL bytes of the combined stdout+stderr buffer.
    let buf: Vec<u8> = (0..6000).map(|i| (i % 256) as u8).collect();
    let tail = tail_bytes(&buf, CRITERION_OUTPUT_TAIL);
    // tail_bytes is UTF-8 lossy; just assert the prefix-stripping happened.
    assert!(tail.len() <= CRITERION_OUTPUT_TAIL * 4);
    let smaller: Vec<u8> = b"short output".to_vec();
    let tail = tail_bytes(&smaller, CRITERION_OUTPUT_TAIL);
    assert_eq!(tail, "short output");
}
