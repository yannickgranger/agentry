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
fn fence_matrix_all_blocker() {
    for (_, _, sev) in FENCE_MATRIX {
        assert_eq!(*sev, Severity::Blocker);
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
