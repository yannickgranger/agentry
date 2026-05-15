//! Tests for `classify_self_review_unapplied` — the pure helper the coder
//! runner uses to decide whether an `all_applied=false` self-review reply is
//! a deliberate disagreement (route to `AwaitingCaptainDecision` via
//! `BriefEvent::CoderDisagreed`) or a bare failure (current
//! `self_review_unapplied` Failed path).

use agentry_role_runtime::{
    classify_self_review_unapplied, SelfReviewClassification, UnappliedVerb,
};

#[test]
fn self_review_with_all_applied_forms_emits_disagreed() {
    let unapplied = vec![
        UnappliedVerb {
            verb: "UPDATE foo.rs:10".into(),
            applied_form: "UPDATE foo.rs:10 (with extra context)".into(),
            rationale: "literal verb context too narrow; widened to capture the surrounding fn"
                .into(),
        },
        UnappliedVerb {
            verb: "DELETE bar.rs:42".into(),
            applied_form: "REPLACE bar.rs:42 with safer guard".into(),
            rationale: "deletion would leave the call site unguarded; replaced instead".into(),
        },
    ];

    let classification = classify_self_review_unapplied(&unapplied);
    match classification {
        SelfReviewClassification::Disagreement(d) => {
            assert_eq!(d.len(), 2);
            assert_eq!(d[0].verb, "UPDATE foo.rs:10");
            assert_eq!(d[0].applied_form, "UPDATE foo.rs:10 (with extra context)");
            assert!(d[0].rationale.starts_with("literal verb context"));
            assert_eq!(d[1].verb, "DELETE bar.rs:42");
            assert_eq!(d[1].applied_form, "REPLACE bar.rs:42 with safer guard");
        }
        other => {
            panic!("expected Disagreement (every entry has applied_form+rationale), got {other:?}")
        }
    }
}

#[test]
fn self_review_with_one_bare_unapplied_emits_failed() {
    let unapplied = vec![
        UnappliedVerb {
            verb: "UPDATE foo.rs:10".into(),
            applied_form: "UPDATE foo.rs:10 (with extra context)".into(),
            rationale: "literal verb context too narrow".into(),
        },
        UnappliedVerb {
            verb: "DELETE bar.rs:42".into(),
            applied_form: String::new(),
            rationale: "couldn't find the symbol".into(),
        },
    ];

    let classification = classify_self_review_unapplied(&unapplied);
    assert_eq!(classification, SelfReviewClassification::BareFailure);
}

#[test]
fn self_review_with_empty_rationale_is_bare_failure() {
    let unapplied = vec![UnappliedVerb {
        verb: "UPDATE foo.rs:10".into(),
        applied_form: "something".into(),
        rationale: String::new(),
    }];

    let classification = classify_self_review_unapplied(&unapplied);
    assert_eq!(classification, SelfReviewClassification::BareFailure);
}

#[test]
fn self_review_with_empty_unapplied_is_bare_failure() {
    let unapplied: Vec<UnappliedVerb> = Vec::new();
    let classification = classify_self_review_unapplied(&unapplied);
    assert_eq!(classification, SelfReviewClassification::BareFailure);
}
