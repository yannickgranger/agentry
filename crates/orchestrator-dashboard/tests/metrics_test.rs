//! Migrated from `src/metrics.rs`'s inline `#[cfg(test)]` block (EPIC #256).
//!
//! Reaches every public helper in `orchestrator_dashboard::metrics`.
//! The original inline block also unit-tested the file-private helpers
//! `render_metrics_table` and `html_escape`. Those are reachable only
//! transitively through `brief_metrics_handler`, which requires a live
//! Redis connection — i.e., they cannot be exercised hermetically from a
//! sibling tests/ crate without promoting the helpers, which the
//! migration brief explicitly forbids ("Do NOT promote private items to
//! `pub` to satisfy tests"). Their behaviour is still covered indirectly
//! every time `brief_metrics_handler` renders against a real brief.

use orchestrator_dashboard::metrics::{
    is_refusal_anomaly, metrics_badge_html, refusal_anomaly_badge_html,
};
use orchestrator_types::{BriefId, Verdict, VerdictKind};
use trace_query::TraceMetric;

#[test]
fn metrics_badge_html_renders_cycles_and_refusals() {
    let m = TraceMetric {
        brief_id: "brf_test".into(),
        compile_cycles: 7,
        refusal_count: 2,
        ..TraceMetric::default()
    };
    let badge = metrics_badge_html(&m);
    assert!(badge.contains("cycles=7"), "badge missing cycles: {badge}");
    assert!(
        badge.contains("refusals=2"),
        "badge missing refusals: {badge}"
    );
    assert!(
        badge.contains("metric-badge"),
        "badge missing class: {badge}"
    );
}

#[test]
fn metrics_badge_html_zero_for_default() {
    let badge = metrics_badge_html(&TraceMetric::default());
    assert!(badge.contains("cycles=0"));
    assert!(badge.contains("refusals=0"));
}

fn synthetic_verdict(kind: VerdictKind, refusal_count: u32) -> Verdict {
    let mut v = Verdict::new(BriefId("brf_test".into()), kind);
    v.refusal_count = refusal_count;
    v
}

#[test]
fn is_refusal_anomaly_true_for_shipped_with_refusals() {
    assert!(is_refusal_anomaly(&synthetic_verdict(
        VerdictKind::Shipped,
        3
    )));
}

#[test]
fn is_refusal_anomaly_false_for_failed_with_refusals() {
    assert!(!is_refusal_anomaly(&synthetic_verdict(
        VerdictKind::Failed,
        5
    )));
}

#[test]
fn is_refusal_anomaly_false_for_shipped_without_refusals() {
    assert!(!is_refusal_anomaly(&synthetic_verdict(
        VerdictKind::Shipped,
        0
    )));
}

#[test]
fn refusal_anomaly_badge_html_renders_warning_for_anomaly() {
    let badge = refusal_anomaly_badge_html(&synthetic_verdict(VerdictKind::Shipped, 4));
    assert!(
        badge.contains("⚠ refusal-on-shipped anomaly"),
        "badge missing warning text: {badge}"
    );
    assert!(
        badge.contains("anomaly-badge"),
        "badge missing class: {badge}"
    );
    assert!(
        badge.contains("shipped with 4"),
        "badge missing tooltip count: {badge}"
    );
}

#[test]
fn refusal_anomaly_badge_html_empty_for_non_anomaly() {
    assert!(refusal_anomaly_badge_html(&synthetic_verdict(VerdictKind::Shipped, 0)).is_empty());
    assert!(refusal_anomaly_badge_html(&synthetic_verdict(VerdictKind::Failed, 7)).is_empty());
}
