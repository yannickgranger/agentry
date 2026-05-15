use orchestrator_types::lifecycle::{decide, Decide, GateConfig, GatePolicy};
use orchestrator_types::EventVerdict;
use std::collections::BTreeMap;

fn cfg(policy: GatePolicy, expected: &[&str]) -> GateConfig {
    GateConfig {
        expected_roles: expected.iter().map(|s| (*s).to_string()).collect(),
        policy,
    }
}

fn rcv(pairs: &[(&str, EventVerdict)]) -> BTreeMap<String, EventVerdict> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
}

// ---- AllMustPass ----------------------------------------------------------

#[test]
fn all_must_pass_empty_is_wait() {
    let g = cfg(GatePolicy::AllMustPass, &["claude", "gemini", "grok"]);
    let r = rcv(&[]);
    assert_eq!(decide(&r, &g), Decide::Wait);
}

#[test]
fn all_must_pass_partial_shipped_is_wait() {
    let g = cfg(GatePolicy::AllMustPass, &["claude", "gemini", "grok"]);
    let r = rcv(&[("claude", EventVerdict::Shipped)]);
    assert_eq!(decide(&r, &g), Decide::Wait);
}

#[test]
fn all_must_pass_all_shipped_is_pass() {
    let g = cfg(GatePolicy::AllMustPass, &["claude", "gemini", "grok"]);
    let r = rcv(&[
        ("claude", EventVerdict::Shipped),
        ("gemini", EventVerdict::Shipped),
        ("grok", EventVerdict::Shipped),
    ]);
    assert_eq!(decide(&r, &g), Decide::Pass);
}

#[test]
fn all_must_pass_one_failed_is_rework() {
    let g = cfg(GatePolicy::AllMustPass, &["claude", "gemini", "grok"]);
    let r = rcv(&[
        ("claude", EventVerdict::Shipped),
        ("gemini", EventVerdict::Failed),
        ("grok", EventVerdict::Shipped),
    ]);
    match decide(&r, &g) {
        Decide::Rework { detail } => assert!(
            detail.contains("gemini"),
            "expected detail to mention gemini, got: {detail}"
        ),
        other => panic!("expected Rework, got {other:?}"),
    }
}

#[test]
fn all_must_pass_one_rejected_is_reject() {
    let g = cfg(GatePolicy::AllMustPass, &["claude", "gemini", "grok"]);
    let r = rcv(&[
        ("claude", EventVerdict::Shipped),
        ("gemini", EventVerdict::Rejected),
    ]);
    match decide(&r, &g) {
        Decide::Reject { detail } => assert!(
            detail.contains("gemini"),
            "expected detail to mention gemini, got: {detail}"
        ),
        other => panic!("expected Reject, got {other:?}"),
    }
}

#[test]
fn all_must_pass_one_escalated_is_reject() {
    let g = cfg(GatePolicy::AllMustPass, &["claude", "gemini", "grok"]);
    let r = rcv(&[("claude", EventVerdict::Escalated)]);
    match decide(&r, &g) {
        Decide::Reject { detail } => assert!(
            detail.contains("claude"),
            "expected detail to mention claude, got: {detail}"
        ),
        other => panic!("expected Reject, got {other:?}"),
    }
}

// ---- FailFast -------------------------------------------------------------

#[test]
fn fail_fast_failed_short_circuits_to_rework() {
    let g = cfg(GatePolicy::FailFast, &["claude", "gemini", "grok"]);
    let r = rcv(&[("claude", EventVerdict::Failed)]);
    match decide(&r, &g) {
        Decide::Rework { detail } => assert!(
            detail.contains("claude"),
            "expected detail to mention claude, got: {detail}"
        ),
        other => panic!("expected Rework, got {other:?}"),
    }
}

#[test]
fn fail_fast_rejected_short_circuits_to_reject() {
    let g = cfg(GatePolicy::FailFast, &["claude", "gemini", "grok"]);
    let r = rcv(&[("claude", EventVerdict::Rejected)]);
    match decide(&r, &g) {
        Decide::Reject { detail } => assert!(
            detail.contains("claude"),
            "expected detail to mention claude, got: {detail}"
        ),
        other => panic!("expected Reject, got {other:?}"),
    }
}

#[test]
fn fail_fast_partial_shipped_is_wait() {
    let g = cfg(GatePolicy::FailFast, &["claude", "gemini", "grok"]);
    let r = rcv(&[
        ("claude", EventVerdict::Shipped),
        ("gemini", EventVerdict::Shipped),
    ]);
    assert_eq!(decide(&r, &g), Decide::Wait);
}

#[test]
fn fail_fast_all_shipped_is_pass() {
    let g = cfg(GatePolicy::FailFast, &["claude", "gemini", "grok"]);
    let r = rcv(&[
        ("claude", EventVerdict::Shipped),
        ("gemini", EventVerdict::Shipped),
        ("grok", EventVerdict::Shipped),
    ]);
    assert_eq!(decide(&r, &g), Decide::Pass);
}

// ---- Majority { threshold_pct: 66 } ---------------------------------------

#[test]
fn majority_66_two_of_three_shipped_is_pass() {
    let g = cfg(GatePolicy::Majority { threshold_pct: 66 }, &["a", "b", "c"]);
    let r = rcv(&[("a", EventVerdict::Shipped), ("b", EventVerdict::Shipped)]);
    assert_eq!(decide(&r, &g), Decide::Pass);
}

#[test]
fn majority_66_one_shipped_is_wait() {
    let g = cfg(GatePolicy::Majority { threshold_pct: 66 }, &["a", "b", "c"]);
    let r = rcv(&[("a", EventVerdict::Shipped)]);
    assert_eq!(decide(&r, &g), Decide::Wait);
}

#[test]
fn majority_66_two_failed_is_reject_unreachable() {
    let g = cfg(GatePolicy::Majority { threshold_pct: 66 }, &["a", "b", "c"]);
    let r = rcv(&[("a", EventVerdict::Failed), ("b", EventVerdict::Failed)]);
    assert!(
        matches!(decide(&r, &g), Decide::Reject { .. }),
        "expected Reject for unreachable threshold with two soft fails"
    );
}

#[test]
fn majority_66_rejected_short_circuits_reject() {
    let g = cfg(GatePolicy::Majority { threshold_pct: 66 }, &["a", "b", "c"]);
    let r = rcv(&[("a", EventVerdict::Rejected)]);
    assert!(
        matches!(decide(&r, &g), Decide::Reject { .. }),
        "expected Reject on hard fail"
    );
}

#[test]
fn majority_66_all_reported_one_shipped_is_rework() {
    let g = cfg(GatePolicy::Majority { threshold_pct: 66 }, &["a", "b", "c"]);
    let r = rcv(&[
        ("a", EventVerdict::Shipped),
        ("b", EventVerdict::Failed),
        ("c", EventVerdict::Failed),
    ]);
    assert!(
        matches!(decide(&r, &g), Decide::Rework { .. }),
        "expected Rework when all reported, soft fails present, threshold not reached"
    );
}

// ---- Majority { threshold_pct: 100 } edge case ----------------------------

#[test]
fn majority_100_all_shipped_is_pass() {
    let g = cfg(
        GatePolicy::Majority { threshold_pct: 100 },
        &["a", "b", "c"],
    );
    let r = rcv(&[
        ("a", EventVerdict::Shipped),
        ("b", EventVerdict::Shipped),
        ("c", EventVerdict::Shipped),
    ]);
    assert_eq!(decide(&r, &g), Decide::Pass);
}

#[test]
fn majority_100_two_shipped_is_wait() {
    let g = cfg(
        GatePolicy::Majority { threshold_pct: 100 },
        &["a", "b", "c"],
    );
    let r = rcv(&[("a", EventVerdict::Shipped), ("b", EventVerdict::Shipped)]);
    assert_eq!(decide(&r, &g), Decide::Wait);
}

#[test]
fn majority_100_two_shipped_one_failed_is_rework() {
    let g = cfg(
        GatePolicy::Majority { threshold_pct: 100 },
        &["a", "b", "c"],
    );
    let r = rcv(&[
        ("a", EventVerdict::Shipped),
        ("b", EventVerdict::Shipped),
        ("c", EventVerdict::Failed),
    ]);
    assert!(
        matches!(decide(&r, &g), Decide::Rework { .. }),
        "expected Rework: all reported, threshold 100 not reached, soft fail present"
    );
}
