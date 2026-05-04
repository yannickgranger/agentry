//! Tests for the coder pure helpers, migrated from the inline
//! `#[cfg(test)] mod tests` block of `src/bin/coder_claude_runner.rs`
//! when the helpers were promoted to the lib (brief X.7c).

use agentry_role_runtime::{
    body_has_verb_syntax, build_coder_prompt, build_rework_banner, build_self_review_prompt,
    collect_blocker_findings, is_v1_plus_topology, mech_finding_warn, parse_brief_context,
    parse_self_review_object, slice_json_object, PriorFinding, SELF_REVIEW_ISSUE_BODY_BUDGET,
};
use orchestrator_types::Severity;
use serde_json::json;

#[test]
fn collect_blocker_findings_filters_by_severity() {
    let bundle = json!({
        "team_context": {
            "messages": [{"payload": {"findings": [
                {"severity": "blocker", "message": "x", "prohibitions": ["a"], "requirements": ["b"]},
                {"severity": "warn", "message": "y"}
            ]}}]
        }
    });
    let v = collect_blocker_findings(&bundle);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].message, "x");
    assert_eq!(v[0].prohibitions, vec!["a"]);
}

#[test]
fn build_rework_banner_joins_with_semicolons_and_keeps_dollar_brace_literal() {
    let f = vec![PriorFinding {
        message: "bad".into(),
        prohibitions: vec!["p1".into(), "p2".into()],
        requirements: vec!["r1".into()],
    }];
    let b = build_rework_banner(&f);
    assert!(b.contains("- BLOCKER: bad"));
    assert!(b.contains("Prohibitions: p1; p2"));
    assert!(b.contains("Requirements: r1"));
    assert!(b.contains("git diff ${base_branch}...HEAD"));
}

#[test]
fn build_coder_prompt_contains_required_anchors() {
    let p = build_coder_prompt(
        "develop",
        "auto/brf_x",
        "",
        "Fix bug",
        "CREATE foo.rs:10",
        "cargo test",
    );
    assert!(p.contains("branch \"develop\""));
    assert!(p.contains("branch \"auto/brf_x\""));
    assert!(p.contains("Task title: Fix bug"));
    assert!(p.contains("CREATE foo.rs:10"));
    assert!(p.contains("cargo test"));
    assert!(p.contains("quality-fast"));
}

#[test]
fn is_v1_plus_topology_matches_v1_through_v99() {
    assert!(is_v1_plus_topology("agentry-self-host-v1"));
    assert!(is_v1_plus_topology("agentry-self-host-v2"));
    assert!(is_v1_plus_topology("agentry-self-host-v9"));
    assert!(is_v1_plus_topology("agentry-self-host-v12"));
    assert!(is_v1_plus_topology("foo-v123"));
}

#[test]
fn is_v1_plus_topology_rejects_v0_and_other_shapes() {
    assert!(!is_v1_plus_topology("agentry-self-host-v0"));
    assert!(!is_v1_plus_topology("agentry-self-host"));
    assert!(!is_v1_plus_topology(""));
    assert!(!is_v1_plus_topology("v1")); // no leading '-'
    assert!(!is_v1_plus_topology("agentry-v")); // empty digits
    assert!(!is_v1_plus_topology("agentry-vx1")); // leading non-digit before v
    assert!(!is_v1_plus_topology("agentry-v01")); // leading zero
    assert!(!is_v1_plus_topology("agentry-self-host-v1-extra")); // suffix after digits
}

#[test]
fn body_has_verb_syntax_recognises_bare_verbs() {
    assert!(body_has_verb_syntax("CREATE foo.rs:10\nUPDATE bar.rs:20"));
    assert!(body_has_verb_syntax("UPDATE foo.rs:10"));
    assert!(body_has_verb_syntax("REPLACE foo.rs:10"));
    assert!(body_has_verb_syntax("DELETE foo.rs:10"));
    assert!(body_has_verb_syntax("MOVE foo.rs:10 -> bar.rs:20"));
}

#[test]
fn body_has_verb_syntax_recognises_numbered_headings() {
    assert!(body_has_verb_syntax("### 1. CREATE foo.rs"));
    assert!(body_has_verb_syntax("### 12. UPDATE foo.rs"));
    assert!(body_has_verb_syntax("preamble\n### 3. MOVE foo.rs"));
}

#[test]
fn body_has_verb_syntax_rejects_free_form() {
    assert!(!body_has_verb_syntax("just a description"));
    assert!(!body_has_verb_syntax(""));
    assert!(!body_has_verb_syntax("create foo.rs"));
    assert!(!body_has_verb_syntax("CREATEx foo.rs"));
    assert!(!body_has_verb_syntax("### CREATE foo.rs"));
    assert!(!body_has_verb_syntax("# CREATE foo.rs"));
}

#[test]
fn slice_json_object_picks_outer_braces() {
    assert_eq!(
        slice_json_object("prefix{\"a\":1}suffix"),
        Some("{\"a\":1}")
    );
    assert_eq!(
        slice_json_object("garbage {\"x\":\"y\"}"),
        Some("{\"x\":\"y\"}")
    );
}

#[test]
fn slice_json_object_returns_none_when_missing() {
    assert_eq!(slice_json_object("no braces"), None);
    assert_eq!(slice_json_object("} only closing"), None);
    assert_eq!(slice_json_object("{ only opening"), None);
}

#[test]
fn parse_self_review_object_extracts_fields() {
    let raw = r#"```json
    {"all_applied": false, "unapplied": ["verb 1", "verb 2"]}
    ```"#;
    let r = parse_self_review_object(raw).expect("parse");
    assert!(!r.all_applied);
    assert_eq!(r.unapplied, vec!["verb 1", "verb 2"]);
}

#[test]
fn parse_self_review_object_defaults_all_applied_when_absent() {
    let raw = r#"{"unapplied": []}"#;
    let r = parse_self_review_object(raw).expect("parse");
    assert!(r.all_applied);
    assert!(r.unapplied.is_empty());
}

#[test]
fn parse_self_review_object_returns_none_for_prose() {
    assert_eq!(parse_self_review_object("just prose"), None);
    assert_eq!(parse_self_review_object("} backwards {"), None);
}

#[test]
fn parse_self_review_object_returns_none_for_array() {
    assert_eq!(parse_self_review_object("[1, 2, 3]"), None);
}

#[test]
fn build_self_review_prompt_includes_diff_and_body_head() {
    let p = build_self_review_prompt("CREATE foo.rs:10", "DIFF_TEXT");
    assert!(p.contains("CREATE foo.rs:10"));
    assert!(p.contains("DIFF_TEXT"));
    assert!(p.contains("all_applied"));
    assert!(p.contains("unapplied"));
    assert!(p.contains("starting with { and ending with }"));
}

#[test]
fn build_self_review_prompt_truncates_long_body() {
    // Use a marker char that doesn't appear elsewhere in the prompt
    // template — the longest run of consecutive markers is the body
    // head, capped at SELF_REVIEW_ISSUE_BODY_BUDGET.
    let body = "Q".repeat(5000);
    let p = build_self_review_prompt(&body, "diff");
    let longest_q_run = p.split(|c: char| c != 'Q').map(str::len).max().unwrap_or(0);
    assert_eq!(longest_q_run, SELF_REVIEW_ISSUE_BODY_BUDGET);
}

#[test]
fn mech_finding_warn_uses_warn_severity() {
    let f = mech_finding_warn("ra-query", "dead-pub", "x is unused");
    assert!(matches!(f.severity, Severity::Warn));
    assert_eq!(f.category, "dead-pub");
}

#[test]
fn parse_brief_context_pulls_all_fields() {
    let bundle = json!({
        "brief": {
            "id": "brf_42",
            "topology": {"name": "agentry-self-host-v0"},
            "payload": {
                "issue_title": "T",
                "issue_body": "BODY",
                "acceptance": "cargo test",
                "base_branch": "develop"
            }
        },
        "permit": {"agent_id": "agt_xyz"},
        "team_context": {"messages": []}
    });
    let ctx = parse_brief_context(&bundle);
    assert_eq!(ctx.brief_id, "brf_42");
    assert_eq!(ctx.branch, "auto/brf_42");
    assert_eq!(ctx.topology_name, "agentry-self-host-v0");
    assert_eq!(ctx.acceptance, "cargo test");
    assert_eq!(ctx.base_branch, "develop");
    assert_eq!(ctx.issue_title, "T");
    assert_eq!(ctx.issue_body, "BODY");
    assert!(ctx.rework_banner.is_empty());
    assert!(ctx.blocker_findings.is_empty());
    assert!(ctx.allowed_tools.is_empty());
}

#[test]
fn parse_brief_context_builds_rework_banner_with_findings() {
    let bundle = json!({
        "brief": {
            "id": "brf_1",
            "topology": {"name": "agentry-self-host-v0"},
            "payload": {"issue_body": "x"}
        },
        "permit": {"agent_id": "a"},
        "team_context": {
            "messages": [{"payload": {"findings": [
                {"severity": "blocker", "message": "rework me", "prohibitions": [], "requirements": []}
            ]}}]
        }
    });
    let ctx = parse_brief_context(&bundle);
    assert!(ctx.rework_banner.contains("REWORK iteration"));
    assert!(ctx.rework_banner.contains("rework me"));
    assert_eq!(ctx.blocker_findings.len(), 1);
}

#[test]
fn parse_brief_context_pulls_allowed_tools_from_permit() {
    let bundle = json!({
        "brief": {"id": "brf_x", "topology": {"name": "agentry-self-host-v0"}, "payload": {}},
        "permit": {"agent_id": "a", "allowed_tools": ["Read", "Bash(*)"]},
        "team_context": {"messages": []}
    });
    let ctx = parse_brief_context(&bundle);
    assert_eq!(
        ctx.allowed_tools,
        vec!["Read".to_string(), "Bash(*)".to_string()]
    );
}
