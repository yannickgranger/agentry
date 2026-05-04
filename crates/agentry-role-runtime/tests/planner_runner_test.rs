//! Tests for the planner pure helpers (EPIC #161 Wave 3). Lives in the
//! test crate (per `arch-ban-inline-cfg-test-in-src.cypher`); the
//! helpers themselves live in
//! `crates/agentry-role-runtime/src/planner.rs` and are reachable via
//! the lib's `pub mod planner;` re-export.

use agentry_role_runtime::planner::{
    build_child_brief, build_planner_prompt, cap_children, discovery_excerpt,
    parse_planner_payload, parse_planner_response, DEFAULT_CHILD_TOPOLOGY, DEFAULT_MAX_CHILDREN,
    DISCOVERY_PROMPT_LIMIT,
};
use serde_json::{json, Value};

#[test]
fn parse_planner_payload_uses_bundle_values() {
    let bundle = json!({
        "brief": {
            "id": "brf_meta_42",
            "payload": {
                "intent": "do the thing",
                "success_criteria": "tests pass",
                "child_topology": "agentry-bugfix-v0",
                "max_children": 5,
                "base_branch": "main",
                "target_repo": "yg/elsewhere",
            },
        },
    });
    let p = parse_planner_payload(&bundle);
    assert_eq!(p.brief_id, "brf_meta_42");
    assert_eq!(p.intent, "do the thing");
    assert_eq!(p.success_criteria, "tests pass");
    assert_eq!(p.child_topology, "agentry-bugfix-v0");
    assert_eq!(p.max_children, 5);
    assert_eq!(p.base_branch, "main");
    assert_eq!(p.target_repo, "yg/elsewhere");
}

#[test]
fn parse_planner_payload_applies_defaults() {
    let bundle = json!({"brief": {"id": "brf_x", "payload": {}}});
    let p = parse_planner_payload(&bundle);
    assert_eq!(p.brief_id, "brf_x");
    assert_eq!(p.intent, "");
    assert_eq!(p.success_criteria, "");
    assert_eq!(p.child_topology, DEFAULT_CHILD_TOPOLOGY);
    assert_eq!(p.max_children, DEFAULT_MAX_CHILDREN);
    assert_eq!(p.base_branch, "develop");
    assert_eq!(p.target_repo, "yg/agentry");
}

#[test]
fn discovery_excerpt_passes_through_when_under_budget() {
    let s = "small payload";
    let (excerpt, truncated, size) = discovery_excerpt(s);
    assert_eq!(excerpt, s);
    assert!(!truncated);
    assert_eq!(size, s.len());
}

#[test]
fn discovery_excerpt_truncates_at_budget() {
    let big = "x".repeat(DISCOVERY_PROMPT_LIMIT * 2);
    let (excerpt, truncated, size) = discovery_excerpt(&big);
    assert_eq!(size, big.len());
    assert!(truncated);
    assert_eq!(excerpt.len(), DISCOVERY_PROMPT_LIMIT);
}

#[test]
fn build_planner_prompt_contains_required_anchors() {
    let p = build_planner_prompt(
        "INTENT_TEXT",
        "SUCCESS_TEXT",
        12345,
        false,
        "DISCO_BLOB",
        "yg/agentry",
        "develop",
        7,
    );
    assert!(p.contains("META-BRIEF INTENT:\nINTENT_TEXT"));
    assert!(p.contains("SUCCESS CRITERIA:\nSUCCESS_TEXT"));
    assert!(p.contains("DISCOVERY (size=12345 bytes, truncated=false):\nDISCO_BLOB"));
    assert!(p.contains("- target_repo: yg/agentry"));
    assert!(p.contains("- base_branch: develop"));
    assert!(p.contains("Cap at\n7 elements"));
    assert!(p.contains("agentry-self-host-v0"));
    assert!(p.contains("agentry-bugfix-v0"));
    assert!(p.contains("agentry-spec-edit-v0"));
    assert!(p.contains("starting with [ and ending with ]"));
}

#[test]
fn build_planner_prompt_marks_truncated_when_over_budget() {
    let p = build_planner_prompt("i", "s", 999_999, true, "head", "yg/r", "develop", 10);
    assert!(p.contains("DISCOVERY (size=999999 bytes, truncated=true):\nhead"));
}

#[test]
fn parse_planner_response_strips_fences_and_returns_array() {
    let raw = "```json\n[{\"title\":\"a\"}, {\"title\":\"b\"}]\n```";
    let v = parse_planner_response(raw).expect("parse");
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].get("title").and_then(Value::as_str), Some("a"));
}

#[test]
fn parse_planner_response_picks_outer_brackets() {
    let raw = "preamble [1, 2, 3] trailing";
    let v = parse_planner_response(raw).expect("parse");
    assert_eq!(v.len(), 3);
    assert_eq!(v[0].as_u64(), Some(1));
    assert_eq!(v[2].as_u64(), Some(3));
}

#[test]
fn parse_planner_response_rejects_object() {
    assert!(parse_planner_response("{\"x\": 1}").is_none());
}

#[test]
fn parse_planner_response_rejects_prose_without_brackets() {
    assert!(parse_planner_response("just prose").is_none());
}

#[test]
fn parse_planner_response_rejects_unparseable() {
    assert!(parse_planner_response("[not, valid, json]").is_none());
}

#[test]
fn cap_children_takes_prefix_when_over_limit() {
    let elems = vec![json!(1), json!(2), json!(3), json!(4)];
    let capped = cap_children(elems, 2);
    assert_eq!(capped, vec![json!(1), json!(2)]);
}

#[test]
fn cap_children_no_op_when_under_limit() {
    let elems = vec![json!(1), json!(2)];
    let capped = cap_children(elems.clone(), 5);
    assert_eq!(capped, elems);
}

#[test]
fn build_child_brief_inlines_planner_metadata() {
    let elem = json!({
        "title": "fix flaky test",
        "verbs": "UPDATE crates/x/tests/foo.rs:42",
        "acceptance": "cargo test --workspace",
        "topology": "agentry-bugfix-v0",
    });
    let v = build_child_brief(
        "brf_meta_99",
        3,
        &elem,
        "agentry-self-host-v0",
        "yg/agentry",
        "develop",
        "2026-05-03T12:34:56+00:00",
    );
    assert_eq!(
        v.get("id").and_then(Value::as_str),
        Some("brf_planner_brf_meta_99_child_3")
    );
    assert_eq!(
        v.pointer("/topology/name").and_then(Value::as_str),
        Some("agentry-bugfix-v0")
    );
    assert_eq!(
        v.pointer("/topology/version").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        v.pointer("/payload/issue_title").and_then(Value::as_str),
        Some("fix flaky test")
    );
    assert_eq!(
        v.pointer("/payload/issue_body").and_then(Value::as_str),
        Some("UPDATE crates/x/tests/foo.rs:42")
    );
    assert_eq!(
        v.pointer("/payload/acceptance").and_then(Value::as_str),
        Some("cargo test --workspace")
    );
    assert_eq!(
        v.pointer("/payload/target_repo").and_then(Value::as_str),
        Some("yg/agentry")
    );
    assert_eq!(
        v.pointer("/payload/base_branch").and_then(Value::as_str),
        Some("develop")
    );
    assert_eq!(
        v.pointer("/payload/pr_title").and_then(Value::as_str),
        Some("auto(planner-brf_meta_99): fix flaky test")
    );
    let pr_body = v
        .pointer("/payload/pr_body")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(pr_body.contains("Authored by planner-claude-agentry from meta-brief brf_meta_99"));
    assert!(pr_body.contains("UPDATE crates/x/tests/foo.rs:42"));
    assert_eq!(
        v.pointer("/budget/max_wall_seconds")
            .and_then(Value::as_u64),
        Some(900)
    );
    assert_eq!(
        v.get("escalation").and_then(Value::as_str),
        Some("autonomous")
    );
    assert_eq!(
        v.get("parent_brief").and_then(Value::as_str),
        Some("brf_meta_99")
    );
    assert_eq!(
        v.get("submitted_by").and_then(Value::as_str),
        Some("planner-claude-agentry-brf_meta_99")
    );
    assert_eq!(
        v.get("submitted_at").and_then(Value::as_str),
        Some("2026-05-03T12:34:56+00:00")
    );
    assert!(v.get("project").map(Value::is_null).unwrap_or(false));
    assert_eq!(
        v.pointer("/payload/issue_number").and_then(Value::as_u64),
        Some(0)
    );
}

#[test]
fn build_child_brief_falls_back_to_default_topology() {
    let elem = json!({"title": "x", "verbs": "UPDATE foo:1", "acceptance": "true"});
    let v = build_child_brief(
        "brf_z",
        0,
        &elem,
        "agentry-self-host-v0",
        "yg/agentry",
        "develop",
        "2026-05-03T00:00:00+00:00",
    );
    assert_eq!(
        v.pointer("/topology/name").and_then(Value::as_str),
        Some("agentry-self-host-v0")
    );
}

#[test]
fn build_child_brief_falls_back_when_topology_empty_string() {
    let elem = json!({"title": "x", "verbs": "v", "acceptance": "true", "topology": ""});
    let v = build_child_brief(
        "brf_z",
        0,
        &elem,
        "agentry-self-host-v0",
        "yg/agentry",
        "develop",
        "2026-05-03T00:00:00+00:00",
    );
    assert_eq!(
        v.pointer("/topology/name").and_then(Value::as_str),
        Some("agentry-self-host-v0")
    );
}
