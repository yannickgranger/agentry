use orchestrator_types::{TargetRepo, TargetRepoParseError};
use std::str::FromStr;

#[test]
fn round_trip_display_matches_input() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(format!("{tr}"), "yg/agentry");
}

#[test]
fn bare_input_binds_placeholder_forge() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(tr.forge(), "agency-default");
}

#[test]
fn forge_qualified_parse_captures_prefix() {
    let tr = TargetRepo::from_str("agency:yg/agentry").expect("parse");
    assert_eq!(tr.forge(), "agency");
    assert_eq!(tr.owner(), "yg");
    assert_eq!(tr.repo(), "agentry");
}

#[test]
fn slug_byte_identity_with_legacy_sanitizer() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(tr.slug(), "yg_agentry");
}

#[test]
fn clone_url_canonical_format() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(
        tr.clone_url("agency.lab:3000"),
        "https://agency.lab:3000/yg/agentry.git"
    );
}

#[test]
fn rejects_invalid_repo_charset() {
    let err = TargetRepo::from_str("yg/agentry@evil.com").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::RepoInvalidChars);
}

#[test]
fn rejects_total_length_over_limit() {
    let owner = "a".repeat(64);
    let repo = "b".repeat(200);
    let input = format!("{owner}/{repo}");
    assert!(input.len() > 200);
    let err = TargetRepo::from_str(&input).expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::TooLong);
}

#[test]
fn rejects_empty_input() {
    let err = TargetRepo::from_str("").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::Empty);
}

#[test]
fn rejects_missing_slash() {
    let err = TargetRepo::from_str("yg-agentry").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::MissingRepo);
}

#[test]
fn cfdb_keyspace_delegates_to_slug() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(tr.cfdb_keyspace(), tr.slug());
}

#[test]
fn display_qualified_includes_forge() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(tr.display_qualified(), "agency-default:yg/agentry");
}

#[test]
fn serde_round_trip_through_string() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    let json = serde_json::to_string(&tr).expect("serialize");
    assert_eq!(json, "\"yg/agentry\"");
    let back: TargetRepo = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, tr);
}

#[test]
fn rejects_owner_starting_with_dot() {
    let err = TargetRepo::from_str(".yg/agentry").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::OwnerStartsWithDotOrDash);
}

#[test]
fn rejects_repo_starting_with_dash() {
    let err = TargetRepo::from_str("yg/-agentry").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::RepoStartsWithDotOrDash);
}
