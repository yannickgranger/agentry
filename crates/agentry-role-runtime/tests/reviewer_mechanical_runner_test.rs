//! Tests for the reviewer-mechanical pure helpers (EPIC #161 Wave 2 final
//! slice). The runner binary itself spawns `sh`, redirects to /tmp, and
//! writes to stdout — all integration-level concerns. These tests cover the
//! pure parsing / truncation layer that lives in the lib crate.
//!
//! Per PR #295 (separate file per arch ban), these live outside `src/` so the
//! inline-cfg-test ban
//! (`.cfdb/queries/arch-ban-inline-cfg-test-in-src.cypher`) has nothing to
//! flag.

use agentry_role_runtime::{build_reviewer_combined, pointer_str_or};
use serde_json::json;

const DEFAULT_BASE_BRANCH: &str = "develop";
const DEFAULT_ACCEPTANCE: &str = "cargo test --workspace";

#[test]
fn default_base_branch() {
    let bundle = json!({"brief": {"payload": {}}});
    let s = pointer_str_or(&bundle, "/brief/payload/base_branch", DEFAULT_BASE_BRANCH);
    assert_eq!(s, "develop");
}

#[test]
fn default_acceptance() {
    let bundle = json!({"brief": {"payload": {}}});
    let s = pointer_str_or(&bundle, "/brief/payload/acceptance", DEFAULT_ACCEPTANCE);
    assert_eq!(s, "cargo test --workspace");
}

#[test]
fn combined_truncation_2000_bytes() {
    let err = "e".repeat(1500);
    let out = "o".repeat(1500);
    let combined = build_reviewer_combined(&err, &out, 2000);
    assert_eq!(combined.len(), 2000);
    assert!(combined.starts_with(&"e".repeat(1500)));
    assert!(combined.contains("\n---stdout---\n"));
}
