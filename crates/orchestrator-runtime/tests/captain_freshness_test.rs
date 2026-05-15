//! Unit tests for `captain freshness`' parser.
//!
//! These exercise `parse_file_refs` directly without spinning up a forge
//! mock — the regex is the load-bearing surface for v1 (the network probe
//! around it is exercised at acceptance time when an operator runs
//! `captain freshness` against a live issue).

use orchestrator_runtime::captain_freshness::{
    classify_pub_name_against_row, parse_file_refs, parse_pub_name_refs, CfdbRow, RefStatus,
};

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

#[test]
fn parse_pub_name_refs_extracts_camel_with_nearest_path() {
    let body = "Update `FooBar` defined in crates/x/src/y.rs:10.";
    let refs = parse_pub_name_refs(body);
    assert_eq!(
        refs,
        vec![("FooBar".to_string(), Some("crates/x/src/y.rs".to_string()))]
    );
}

#[test]
fn parse_pub_name_refs_skips_english_allowlist() {
    let body = "Remember `TODO` and `FIXME` are just prose markers.";
    let refs = parse_pub_name_refs(body);
    assert!(
        refs.is_empty(),
        "expected English allowlist (TODO/FIXME) to be skipped; got {refs:?}"
    );
}

#[test]
fn classify_pub_name_against_row_renamed_on_path_mismatch() {
    let row = CfdbRow {
        qname: "crate::y::FooBar".to_string(),
        file: "crates/x/src/z.rs".to_string(),
    };
    let status = classify_pub_name_against_row("FooBar", Some("crates/x/src/y.rs"), Some(&row));
    assert_eq!(
        status,
        RefStatus::Renamed {
            name: "FooBar".to_string(),
            expected_file: "crates/x/src/y.rs".to_string(),
            actual_file: "crates/x/src/z.rs".to_string(),
        }
    );
}
