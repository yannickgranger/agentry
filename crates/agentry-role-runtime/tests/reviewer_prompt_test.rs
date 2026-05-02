//! Tests for `build_review_prompt`, migrated from the inline
//! `#[cfg(test)] mod tests` block of `src/bin/reviewer_claude_runner.rs`
//! when the helper was promoted to the lib (brief X.7c).

use agentry_role_runtime::build_review_prompt;

#[test]
fn build_review_prompt_includes_diff_and_title() {
    let prompt = build_review_prompt("develop", "Fix bug", "BODY", "DIFF_TEXT");
    assert!(prompt.contains("TITLE: Fix bug"));
    assert!(prompt.contains("DIFF_TEXT"));
    assert!(prompt.contains("Output EXACTLY a JSON array"));
    assert!(!prompt.contains("--- Mechanical findings"));
}
