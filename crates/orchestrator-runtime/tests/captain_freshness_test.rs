//! Unit tests for `captain freshness`' parser.
//!
//! These exercise `parse_file_refs` directly without spinning up a forge
//! mock — the regex is the load-bearing surface for v1 (the network probe
//! around it is exercised at acceptance time when an operator runs
//! `captain freshness` against a live issue).

use orchestrator_runtime::captain_freshness::parse_file_refs;

#[test]
fn parse_extracts_file_line_refs() {
    let body = "see crates/foo/src/bar.rs:42 and crates/baz/Cargo.toml";
    let refs = parse_file_refs(body);
    assert_eq!(
        refs,
        vec![
            ("crates/foo/src/bar.rs".to_string(), Some(42)),
            ("crates/baz/Cargo.toml".to_string(), None),
        ]
    );
}

#[test]
fn parse_dedupes_repeated_refs() {
    let body = "crates/foo.rs:10 then later again crates/foo.rs:10 — same ref";
    let refs = parse_file_refs(body);
    assert_eq!(refs, vec![("crates/foo.rs".to_string(), Some(10))]);
}

#[test]
fn parse_ignores_non_path_words() {
    let body = "the README.md is in the root";
    let refs = parse_file_refs(body);
    assert!(
        refs.is_empty(),
        "expected no matches for a bare README.md without crates/ prefix; got {refs:?}"
    );
}
