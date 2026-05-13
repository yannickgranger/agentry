//! Regression guard for `mark_failed` in `daemon_resume.rs`: the function
//! must not re-introduce a redundant `let brief_id = record.brief_id.clone();`
//! local binding. The earlier debt mop-up removed it — all `Display`
//! formats now read `record.brief_id.0` directly (by reference), and only
//! the `BriefStateRecord` struct-construction site clones the brief_id
//! (because the field is owned).
//!
//! This is a source-text check, mirroring the pattern from
//! `lifecycle_observability_test.rs` (regression-guard via file read).
//! Runtime exercise of `mark_failed` lives in the live-Redis integration
//! suite at `tests/daemon_resume.rs`.

use std::fs;
use std::path::PathBuf;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Carve `mark_failed`'s body out of the source file. Other functions in
/// daemon_resume.rs legitimately bind `let brief_id = record.brief_id.clone();`
/// (notably the `resume_orphans` per-record loop), so the regression check
/// must be scoped to `mark_failed` alone.
fn mark_failed_body(src: &str) -> &str {
    let signature = "async fn mark_failed(";
    let fn_start = src
        .find(signature)
        .expect("locate `async fn mark_failed(` in daemon_resume.rs");
    let body_open = fn_start
        + src[fn_start..]
            .find('{')
            .expect("locate opening `{` of `mark_failed`");
    // Walk braces forward to find the matching close — the function's body
    // contains nested `{}` blocks (struct literal, match arms, format!).
    let bytes = src.as_bytes();
    let mut depth: i32 = 0;
    let mut i = body_open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[body_open..=i];
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced braces while carving `mark_failed` body");
}

#[test]
fn mark_failed_uses_record_brief_id_not_local_clone() {
    let path = crate_root().join("src").join("daemon_resume.rs");
    let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let body = mark_failed_body(&src);
    assert!(
        !body.contains("let brief_id = record.brief_id.clone();"),
        "daemon_resume.rs re-introduced the redundant `let brief_id = record.brief_id.clone();` \
         binding in `mark_failed`; read `record.brief_id.0` directly in the Display formats and \
         clone only at the struct-construction site"
    );
}
