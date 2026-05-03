//! Tests for the archaeologist pure helpers (EPIC #161 Wave 3). Lives in
//! the test crate (per `arch-ban-inline-cfg-test-in-src.cypher`); the
//! helpers themselves live in
//! `crates/agentry-role-runtime/src/archaeologist.rs` and are reachable
//! via the lib's `pub mod archaeologist;` re-export.

use agentry_role_runtime::archaeologist::{
    build_archaeologist_prompt, parse_cfdb_counts, parse_discovery_object, parse_discovery_seeds,
    GRAPH_SPECS_HEAD,
};
use serde_json::json;

#[test]
fn parse_cfdb_counts_extracts_nodes_and_edges() {
    let log = "starting...\n[INFO] extract: 123 nodes, 456 edges done\nfinishing\n";
    assert_eq!(parse_cfdb_counts(log), (123, 456));
}

#[test]
fn parse_cfdb_counts_takes_last_match() {
    let log = "extract: 1 nodes, 2 edges\nintermediate\nextract: 99 nodes, 88 edges\n";
    assert_eq!(parse_cfdb_counts(log), (99, 88));
}

#[test]
fn parse_cfdb_counts_zero_when_pattern_absent() {
    assert_eq!(parse_cfdb_counts(""), (0, 0));
    assert_eq!(parse_cfdb_counts("nothing matches here\n"), (0, 0));
    assert_eq!(parse_cfdb_counts("extract: not a number nodes\n"), (0, 0));
}

#[test]
fn parse_cfdb_counts_nodes_only_when_edges_missing() {
    // Bash sed `'s/.*nodes, ([0-9]+) edges.*/\1/p'` produces empty when
    // the comma+digits+edges pattern is absent — defaults to 0.
    let log = "extract: 50 nodes only, no edges field here\n";
    assert_eq!(parse_cfdb_counts(log), (50, 0));
}

#[test]
fn parse_discovery_seeds_pulls_string_array() {
    let bundle = json!({
        "brief": {
            "payload": {
                "discovery_seeds": [
                    "MATCH (n) RETURN n LIMIT 1",
                    "MATCH (m:Item) RETURN m.qname"
                ]
            }
        }
    });
    let seeds = parse_discovery_seeds(&bundle);
    assert_eq!(
        seeds,
        vec![
            "MATCH (n) RETURN n LIMIT 1".to_string(),
            "MATCH (m:Item) RETURN m.qname".to_string(),
        ]
    );
}

#[test]
fn parse_discovery_seeds_empty_when_missing() {
    let bundle = json!({"brief": {"payload": {}}});
    assert!(parse_discovery_seeds(&bundle).is_empty());
}

#[test]
fn parse_discovery_seeds_drops_non_string_entries() {
    let bundle = json!({
        "brief": {
            "payload": {
                "discovery_seeds": ["a", 42, null, "b"]
            }
        }
    });
    let seeds = parse_discovery_seeds(&bundle);
    assert_eq!(seeds, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn build_archaeologist_prompt_contains_required_anchors() {
    let p = build_archaeologist_prompt(
        "INTENT_TEXT",
        "SUCCESS_TEXT",
        100,
        250,
        "GRAPH_SPECS_OUTPUT",
        "[]",
    );
    assert!(p.contains("INTENT:\nINTENT_TEXT"));
    assert!(p.contains("SUCCESS CRITERIA:\nSUCCESS_TEXT"));
    assert!(p.contains("nodes=100, edges=250"));
    assert!(p.contains("GRAPH_SPECS_OUTPUT"));
    assert!(p.contains("SEED-QUERY RESULTS (JSON):\n[]"));
    assert!(p.contains("\"cfdb\": {\"nodes\": 100, \"edges\": 250}"));
    assert!(p.contains("\"seed_queries\": []"));
    assert!(p.contains("starting with { and ending with }"));
}

#[test]
fn build_archaeologist_prompt_truncates_graph_specs_to_head_budget() {
    let big = "G".repeat(GRAPH_SPECS_HEAD * 2);
    let p = build_archaeologist_prompt("i", "s", 0, 0, &big, "[]");
    let longest_g_run = p.split(|c: char| c != 'G').map(str::len).max().unwrap_or(0);
    assert_eq!(longest_g_run, GRAPH_SPECS_HEAD);
}

#[test]
fn parse_discovery_object_strips_fences_and_returns_object() {
    let raw = "```json\n{\"intent\":\"x\",\"summary\":\"y\"}\n```";
    let v = parse_discovery_object(raw).expect("parse");
    assert_eq!(v.get("intent").and_then(|s| s.as_str()), Some("x"));
    assert_eq!(v.get("summary").and_then(|s| s.as_str()), Some("y"));
}

#[test]
fn parse_discovery_object_picks_outer_braces() {
    let raw = "garbage {\"a\":1, \"nested\":{\"b\":2}} trailing";
    let v = parse_discovery_object(raw).expect("parse");
    assert_eq!(v.get("a").and_then(|n| n.as_u64()), Some(1));
    assert_eq!(v.pointer("/nested/b").and_then(|n| n.as_u64()), Some(2),);
}

#[test]
fn parse_discovery_object_rejects_array() {
    assert!(parse_discovery_object("[1, 2, 3]").is_none());
}

#[test]
fn parse_discovery_object_rejects_prose_without_braces() {
    assert!(parse_discovery_object("just prose, no JSON").is_none());
}

#[test]
fn parse_discovery_object_rejects_unparseable() {
    assert!(parse_discovery_object("{not valid json}").is_none());
}
