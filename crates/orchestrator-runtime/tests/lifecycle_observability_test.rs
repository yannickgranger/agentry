//! Regression guard for the `#[tracing::instrument]` attributes on the
//! per-brief lifecycle functions. The attributes propagate `brief = ...`
//! (and `disposition = ...` for cleanup) into every nested log line in
//! those functions; accidental removal during a future refactor would
//! silently break multi-brief log timeline observability. This test
//! parses the source files as text rather than exercising the tracing
//! subscriber at runtime (which requires more elaborate test plumbing).

use std::fs;
use std::path::PathBuf;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn lifecycle_driver_has_instrument_spans() {
    let path = crate_root().join("src").join("lifecycle_driver.rs");
    let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    assert!(
        src.contains("#[tracing::instrument(skip_all, fields(brief ="),
        "lifecycle_driver.rs lost the `#[tracing::instrument(skip_all, fields(brief = ...)]` \
         attribute on a per-brief lifecycle function; restore it to keep brief_id propagating \
         into nested log calls"
    );
    assert!(
        src.contains("disposition = %disposition.label()"),
        "lifecycle_driver.rs lost the `disposition = %disposition.label()` field on \
         `cleanup_brief_at`; restore it so the cleanup body's logs stay tagged with the \
         disposition label (Failed / ShippedNoOp)"
    );
}

#[test]
fn daemon_resume_has_instrument_spans() {
    let path = crate_root().join("src").join("daemon_resume.rs");
    let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    assert!(
        src.contains("#[tracing::instrument(skip_all, fields(brief ="),
        "daemon_resume.rs lost the `#[tracing::instrument(skip_all, fields(brief = ...)]` \
         attribute on `reattach_brief` or `mark_failed`; restore it to keep brief_id \
         propagating into nested log calls"
    );
}
