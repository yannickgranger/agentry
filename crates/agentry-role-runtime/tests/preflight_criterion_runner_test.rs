//! Tests for the preflight-criterion pure helpers (EPIC #161
//! wave-bash port). The runner binary itself spawns `bash` and writes
//! to stdout — both are integration-level concerns. These tests cover
//! the pure parsing / smell-detection layer that lives in the lib
//! crate.
//!
//! Per PR #295 (separate file per arch ban), these live outside `src/`
//! so the inline-cfg-test ban
//! (`.cfdb/queries/arch-ban-inline-cfg-test-in-src.cypher`) has nothing
//! to flag.

use agentry_role_runtime::{
    first_blocking_preflight_smell, pointer_str, smell_grep_v_mod_tests,
    smell_huge_baseline_zero_expected, smell_wc_l_without_cfg_test, split_criterion,
    PREFLIGHT_CATEGORY, PREFLIGHT_SMELL_CAUSE, PREFLIGHT_TOOL,
};
use orchestrator_types::{FindingOrigin, Severity};
use serde_json::json;

// -- bundle parsing ----------------------------------------------------

#[test]
fn pointer_str_extracts_success_criteria_and_target_repo() {
    let bundle = json!({
        "brief": {
            "payload": {
                "success_criteria": "rg -c 'unwrap' src | wc -l : 0",
                "target_repo": "yg/agentry"
            }
        }
    });
    assert_eq!(
        pointer_str(&bundle, "/brief/payload/success_criteria"),
        "rg -c 'unwrap' src | wc -l : 0",
    );
    assert_eq!(
        pointer_str(&bundle, "/brief/payload/target_repo"),
        "yg/agentry",
    );
}

#[test]
fn pointer_str_returns_empty_when_success_criteria_missing() {
    let bundle = json!({"brief": {"payload": {}}});
    assert_eq!(pointer_str(&bundle, "/brief/payload/success_criteria"), "");
}

#[test]
fn pointer_str_returns_empty_when_payload_missing() {
    let bundle = json!({"brief": {}});
    assert_eq!(pointer_str(&bundle, "/brief/payload/success_criteria"), "");
}

// -- split_criterion ---------------------------------------------------

#[test]
fn split_criterion_splits_on_first_space_colon_space() {
    let (cmd, expected) = split_criterion("rg -c foo src | wc -l : 0").expect("separator present");
    assert_eq!(cmd, "rg -c foo src | wc -l");
    assert_eq!(expected, "0");
}

#[test]
fn split_criterion_only_splits_on_first_separator() {
    // The expected value can itself contain " : " — only the first
    // occurrence is a separator (per bash `${criterion%% : *}`).
    let (cmd, expected) = split_criterion("echo foo : bar : baz").expect("separator present");
    assert_eq!(cmd, "echo foo");
    assert_eq!(expected, "bar : baz");
}

#[test]
fn split_criterion_trims_expected_whitespace() {
    let (_, expected) = split_criterion("cmd :   42  ").expect("separator present");
    assert_eq!(expected, "42");
}

#[test]
fn split_criterion_returns_none_when_separator_absent() {
    assert!(split_criterion("just-a-command").is_none());
    // Colon without surrounding spaces does NOT count as separator.
    assert!(split_criterion("foo:bar").is_none());
    assert!(split_criterion("foo :bar").is_none());
    assert!(split_criterion("foo: bar").is_none());
}

// -- smell 1: huge baseline vs zero expected ---------------------------

#[test]
fn smell_huge_baseline_fires_when_expected_zero_baseline_huge_with_wc_l() {
    let f = smell_huge_baseline_zero_expected("rg foo src | wc -l", "150", "0")
        .expect("smell 1 should fire on expected=0 baseline=150 wc -l");
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(f.category, PREFLIGHT_CATEGORY);
    assert!(f.message.contains("baseline (150)"));
    assert!(f.message.contains("expected (0)"));
    match &f.origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, PREFLIGHT_TOOL);
            assert_eq!(rule, &None);
        }
        _ => panic!("expected Mechanical origin"),
    }
}

#[test]
fn smell_huge_baseline_silent_when_baseline_at_threshold() {
    // `[ "$baseline" -gt 100 ]` — strictly greater, so 100 must NOT fire.
    assert!(smell_huge_baseline_zero_expected("foo | wc -l", "100", "0").is_none());
    assert!(smell_huge_baseline_zero_expected("foo | wc -l", "101", "0").is_some());
}

#[test]
fn smell_huge_baseline_silent_when_expected_nonzero() {
    assert!(smell_huge_baseline_zero_expected("foo | wc -l", "999", "5").is_none());
}

#[test]
fn smell_huge_baseline_silent_when_no_wc_l() {
    assert!(smell_huge_baseline_zero_expected("rg foo src", "999", "0").is_none());
}

#[test]
fn smell_huge_baseline_silent_when_baseline_non_numeric() {
    assert!(smell_huge_baseline_zero_expected("foo | wc -l", "abc", "0").is_none());
}

#[test]
fn smell_huge_baseline_silent_when_expected_non_numeric() {
    // Bash `grep -qE '^[0-9]+$'` rejects non-numeric expected.
    assert!(smell_huge_baseline_zero_expected("foo | wc -l", "999", "zero").is_none());
}

// -- smell 2: grep -v 'mod tests' --------------------------------------

#[test]
fn smell_grep_v_mod_tests_fires_on_canonical_broken_filter() {
    let cmd = "rg -c unwrap src | grep -v 'mod tests' | wc -l";
    let f = smell_grep_v_mod_tests(cmd).expect("smell 2 should fire on canonical broken filter");
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(f.category, PREFLIGHT_CATEGORY);
    assert!(f.message.contains("grep -v 'mod tests'"));
    assert!(f.message.contains("ra-query") || f.message.contains("cfdb"));
}

#[test]
fn smell_grep_v_mod_tests_silent_on_clean_command() {
    assert!(smell_grep_v_mod_tests("rg -c unwrap src").is_none());
}

#[test]
fn smell_grep_v_mod_tests_silent_on_double_quotes() {
    // Bash literal match is single-quoted; a double-quoted variant is
    // not the canonical broken pattern. (Whether to flag it is a
    // future smell rule; this test pins current parity with bash.)
    assert!(smell_grep_v_mod_tests(r#"foo | grep -v "mod tests""#).is_none());
}

// -- smell 3: wc -l without #[cfg(test)] -------------------------------

#[test]
fn smell_wc_l_without_cfg_test_fires_on_naive_count() {
    let f = smell_wc_l_without_cfg_test("rg unwrap src | wc -l")
        .expect("smell 3 should fire on naive wc -l");
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(f.category, PREFLIGHT_CATEGORY);
    assert!(f.message.contains("wc -l"));
    assert!(f.message.contains("test-scope"));
}

#[test]
fn smell_wc_l_without_cfg_test_silent_when_cfg_test_present() {
    let cmd = "rg unwrap src | rg -v '#[cfg(test)]' | wc -l";
    assert!(smell_wc_l_without_cfg_test(cmd).is_none());
}

#[test]
fn smell_wc_l_without_cfg_test_silent_when_no_wc_l() {
    assert!(smell_wc_l_without_cfg_test("rg -c unwrap src").is_none());
}

// -- finding-shape origin attribution ---------------------------------

#[test]
fn all_smell_findings_use_mechanical_origin_with_preflight_tool() {
    let findings = vec![
        smell_huge_baseline_zero_expected("foo | wc -l", "999", "0").expect("smell 1 fires"),
        smell_grep_v_mod_tests("foo | grep -v 'mod tests'").expect("smell 2 fires"),
        smell_wc_l_without_cfg_test("foo | wc -l").expect("smell 3 fires"),
    ];
    for f in &findings {
        match &f.origin {
            FindingOrigin::Mechanical { tool, rule } => {
                assert_eq!(tool, PREFLIGHT_TOOL);
                assert_eq!(rule, &None);
            }
            _ => panic!("smell findings must be Mechanical-origin (matches bash)"),
        }
        assert_eq!(f.category, PREFLIGHT_CATEGORY);
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.file.is_none());
        assert!(f.line.is_none());
        assert!(f.suggested_fix.is_none());
        assert!(f.prohibitions.is_empty());
        assert!(f.requirements.is_empty());
    }
}

// -- brief 84b-1b: smell promotion to terminal Failed -----------------
//
// These tests pin the runner-level decision: which smells block (1+2),
// which stay advisory (3), which order applies (1 before 2), and that
// the `DoneReason.cause` discriminant the runner emits is exactly the
// string the daemon's trace translator will fold into
// `BriefEvent::PreflightSmellDetected`.

#[test]
fn preflight_smell_cause_constant_is_preflight_smell() {
    // The daemon's trace translator (84b-2) matches on this exact
    // string. Pin it so a rename here forces the translator update.
    assert_eq!(PREFLIGHT_SMELL_CAUSE, "preflight_smell");
}

#[test]
fn smell_1_fires_done_failed_with_preflight_smell_cause() {
    // Fixture criterion `rg ... | wc -l : 0` against a workspace that
    // produces baseline=200. Smell-1 (huge baseline + zero expected)
    // fires; the runner emits this finding then `done failed` with
    // cause "preflight_smell".
    let f = first_blocking_preflight_smell("rg unwrap src | wc -l", "200", "0")
        .expect("smell-1 must fire on baseline=200, expected=0, wc -l cmd");
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(f.category, PREFLIGHT_CATEGORY);
    assert!(f.message.contains("baseline (200)"));
    assert!(f.message.contains("expected (0)"));
    match &f.origin {
        FindingOrigin::Mechanical { tool, .. } => assert_eq!(tool, PREFLIGHT_TOOL),
        _ => panic!("smell-1 finding must be Mechanical-origin"),
    }
    // Cause string the runner pairs with this finding.
    assert_eq!(PREFLIGHT_SMELL_CAUSE, "preflight_smell");
}

#[test]
fn smell_2_fires_done_failed_with_preflight_smell_cause() {
    // Fixture criterion containing literal `grep -v 'mod tests'`.
    // Smell-2 fires (smell-1 silent: baseline=5 ≤ 100); the runner
    // emits this finding then `done failed` with cause
    // "preflight_smell".
    let f = first_blocking_preflight_smell("rg unwrap src | grep -v 'mod tests' | wc -l", "5", "0")
        .expect("smell-2 must fire on canonical broken filter when smell-1 is silent");
    assert_eq!(f.severity, Severity::Warn);
    assert_eq!(f.category, PREFLIGHT_CATEGORY);
    assert!(f.message.contains("grep -v 'mod tests'"));
    assert_eq!(PREFLIGHT_SMELL_CAUSE, "preflight_smell");
}

#[test]
fn clean_criterion_emits_baseline_match_then_done_shipped() {
    // Fixture criterion with no smells. `first_blocking_preflight_smell`
    // returns None, so the runner skips the failed-emit branch and
    // proceeds to `emit_done(Shipped, None)`. NO Warn finding is
    // emitted (smell-3 is also silent here — `echo` does not contain
    // `wc -l`). This pins the happy path the v2 attempt regressed.
    assert!(first_blocking_preflight_smell("echo ok", "ok", "ok").is_none());
    // Smell-3 (advisory) stays silent on a criterion without `wc -l`.
    assert!(smell_wc_l_without_cfg_test("echo ok").is_none());
}

#[test]
fn first_smell_wins() {
    // Criterion matching BOTH smell-1 (wc -l + huge baseline + zero
    // expected) AND smell-2 (`grep -v 'mod tests'`). Runner checks
    // smell-1 first per the existing order, so the helper returns
    // smell-1's finding — the runner emits exactly ONE finding then
    // returns without ever evaluating smell-2.
    let cmd = "rg unwrap src | grep -v 'mod tests' | wc -l";
    // Both helpers detect their respective smells in isolation.
    assert!(smell_huge_baseline_zero_expected(cmd, "200", "0").is_some());
    assert!(smell_grep_v_mod_tests(cmd).is_some());
    // The blocking-smell decision returns smell-1's finding (its
    // message is the baseline-vs-expected wording, not the
    // grep-v-mod-tests wording).
    let f = first_blocking_preflight_smell(cmd, "200", "0")
        .expect("at least one blocking smell must fire");
    assert!(
        f.message.contains("baseline (200)"),
        "smell-1 must win — got: {}",
        f.message
    );
    assert!(
        !f.message.contains("grep -v 'mod tests'"),
        "smell-2's finding must not be returned when smell-1 fires first — got: {}",
        f.message
    );
}
