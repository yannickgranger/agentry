//! Pure helper for computing the on-disk cache directory used by
//! `captain ground --target-repo`. Kept as a no-I/O, no-env-access
//! function so callers can pass explicit `xdg_cache_home` and `home`
//! values from tests without mutating process environment variables.
//!
//! Branch order matches the brief:
//!   1. `xdg_cache_home` set → `<xdg>/captain/ground/<slug>-<rev>/`
//!   2. `home` set          → `<home>/.cache/captain/ground/<slug>-<rev>/`
//!   3. neither              → `/tmp/captain-ground/<slug>-<rev>/`

use std::path::PathBuf;

pub fn captain_ground_cache_dir(
    slug: &str,
    rev: &str,
    xdg_cache_home: Option<String>,
    home: Option<String>,
) -> PathBuf {
    let leaf = format!("{slug}-{rev}");
    if let Some(xdg) = xdg_cache_home {
        return PathBuf::from(xdg).join("captain").join("ground").join(leaf);
    }
    if let Some(h) = home {
        return PathBuf::from(h)
            .join(".cache")
            .join("captain")
            .join("ground")
            .join(leaf);
    }
    PathBuf::from("/tmp/captain-ground").join(leaf)
}
