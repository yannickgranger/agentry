//! Hermetic tests for `captain_ground_cache_dir`. The helper is pure
//! (no I/O, no env access) so each case can pass explicit `xdg` and
//! `home` arguments without mutating process environment variables —
//! safe to run in parallel under `cargo test`.

use orchestrator_runtime::captain_ground_cache::captain_ground_cache_dir;
use std::path::PathBuf;

#[test]
fn captain_ground_cache_dir_uses_xdg_when_present() {
    let got = captain_ground_cache_dir(
        "yg_glean",
        "develop",
        Some("/cache".to_string()),
        Some("/home/anyone".to_string()),
    );
    assert_eq!(got, PathBuf::from("/cache/captain/ground/yg_glean-develop"));
}

#[test]
fn captain_ground_cache_dir_falls_back_to_home_dot_cache() {
    let got = captain_ground_cache_dir("yg_glean", "develop", None, Some("/home/yg".to_string()));
    assert_eq!(
        got,
        PathBuf::from("/home/yg/.cache/captain/ground/yg_glean-develop")
    );
}

#[test]
fn captain_ground_cache_dir_last_resort() {
    let got = captain_ground_cache_dir("yg_glean", "develop", None, None);
    assert_eq!(got, PathBuf::from("/tmp/captain-ground/yg_glean-develop"));
}
