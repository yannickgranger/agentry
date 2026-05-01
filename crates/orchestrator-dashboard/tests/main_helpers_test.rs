//! Migrated from `src/main.rs`'s inline `#[cfg(test)]` block (EPIC #256).
//! `resolve_webhook_secret` now lives in
//! `orchestrator_dashboard::resolve_webhook_secret` (lifted from main.rs
//! to the lib so this file can reach it).

use orchestrator_dashboard::resolve_webhook_secret;

#[test]
fn resolve_webhook_secret_passes_through_explicit_value() {
    let got = resolve_webhook_secret(Some("foo".into())).expect("resolve");
    assert_eq!(got, Some("foo".into()));
}
