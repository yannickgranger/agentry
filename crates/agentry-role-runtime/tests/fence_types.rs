use agentry_role_runtime::{FenceKind, Threshold, UnwrapSeverity, FENCE_MATRIX};
use orchestrator_types::review::Severity;

#[test]
fn fence_matrix_has_five_entries() {
    assert_eq!(FENCE_MATRIX.len(), 5);
}

#[test]
fn fence_matrix_covers_every_kind() {
    let kinds: Vec<FenceKind> = FENCE_MATRIX.iter().map(|(k, _, _)| *k).collect();
    assert!(kinds.contains(&FenceKind::ClonesInLoop));
    assert!(kinds.contains(&FenceKind::CloneProd));
    assert!(kinds.contains(&FenceKind::Complexity));
    assert!(kinds.contains(&FenceKind::Unwraps));
    assert!(kinds.contains(&FenceKind::CallersZero));
}

#[test]
fn fence_matrix_severity_layout() {
    // Metric fences (clones/complexity/unwraps) scan whole CHANGED FILES,
    // not new-lines-only — demoted to Warn until scope-by-diff lands so
    // they don't tank briefs that touch functions in files carrying
    // legacy debt. CallersZero stays Blocker — it is a new-pub-zero-callers
    // split-brain signal on NEW code, not a metric on pre-existing code.
    for (kind, _, sev) in FENCE_MATRIX {
        let expected = match kind {
            FenceKind::CallersZero => Severity::Blocker,
            FenceKind::ClonesInLoop
            | FenceKind::CloneProd
            | FenceKind::Complexity
            | FenceKind::Unwraps => Severity::Warn,
        };
        assert_eq!(*sev, expected, "unexpected severity for {kind:?}");
    }
}

#[test]
fn unwrap_severity_orders_high_above_medium() {
    assert!(UnwrapSeverity::High > UnwrapSeverity::Medium);
    assert!(UnwrapSeverity::Critical > UnwrapSeverity::High);
}

#[test]
fn complexity_threshold_is_fifteen() {
    let entry = FENCE_MATRIX
        .iter()
        .find(|(k, _, _)| *k == FenceKind::Complexity)
        .expect("complexity in matrix");
    assert_eq!(entry.1, Threshold::GreaterThan(15));
}
