//! Helper for resolving `captain dispatch`'s required env from canonical
//! on-disk locations.
//!
//! `captain dispatch <brief.json>` needs three pieces of env to shell out
//! to `orchestrator submit`:
//!
//!   * `AGENTRY_REDIS_PASSWORD`
//!   * `AGENTRY_REDIS__URL`
//!   * `AGENTRY_SIGNING__KEY_PATH`
//!
//! When the operator's local install follows the agentry layout convention,
//! these values live at canonical paths under `$HOME/.config/agentry/`.
//! Historically captain dispatch required all three to be set in the
//! environment up-front: the operator would see one missing-env error,
//! fix it, retry, see the next, and so on. This module collapses that
//! into one disk-probe pass with a single missing-config block on failure.
//!
//! The helper lives in the library (rather than alongside `cmd_dispatch`
//! in the captain bin) so unit tests can drive it directly without
//! spawning a subprocess.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// Populate the three env vars `captain dispatch` requires for
/// `orchestrator submit`, falling back to canonical on-disk locations
/// under `$HOME/.config/agentry/` for any piece that is unset.
///
/// Probe order matches the brief that introduced this helper:
///   1. `AGENTRY_REDIS_PASSWORD` — if unset, read
///      `$HOME/.config/agentry/redis.password`, trim whitespace, and set.
///   2. `AGENTRY_REDIS__URL` — if unset and the password is now resolvable,
///      synthesize `redis://:<password>@127.0.0.1:6380` and set.
///   3. `AGENTRY_SIGNING__KEY_PATH` — if unset, default to
///      `$HOME/.config/agentry/signing.key`. The env var is always set in
///      this branch (the value is a path; downstream `orchestrator` code
///      surfaces the real failure if the file is missing), but the file's
///      existence is verified up front so the operator gets actionable
///      guidance instead of a generic IO error later.
///
/// If any piece cannot be resolved, the helper emits a single
/// operator-facing block on stderr listing every missing piece with its
/// canonical location plus a one-line hint, then returns Err so
/// `cmd_dispatch` aborts before reaching `orchestrator submit` with a
/// half-set environment.
pub fn populate_env_from_disk() -> Result<()> {
    let have_password_env = std::env::var("AGENTRY_REDIS_PASSWORD").is_ok();
    let have_url_env = std::env::var("AGENTRY_REDIS__URL").is_ok();
    let have_signing_env = std::env::var("AGENTRY_SIGNING__KEY_PATH").is_ok();

    if have_password_env && have_url_env && have_signing_env {
        return Ok(());
    }

    let home = std::env::var("HOME")
        .map_err(|_| anyhow!("HOME is unset; cannot resolve canonical agentry config paths"))?;
    let cfg_dir = PathBuf::from(home).join(".config").join("agentry");
    let redis_password_path = cfg_dir.join("redis.password");
    let signing_key_path = cfg_dir.join("signing.key");

    let mut missing: Vec<String> = Vec::new();
    let mut have_password = have_password_env;

    if !have_password {
        match std::fs::read_to_string(&redis_password_path) {
            Ok(raw) => {
                std::env::set_var("AGENTRY_REDIS_PASSWORD", raw.trim());
                have_password = true;
            }
            Err(_) => {
                missing.push(format!(
                    "AGENTRY_REDIS_PASSWORD — expected at {} \
                     (create with: `just dev-redis-up`, which generates this file once)",
                    redis_password_path.display(),
                ));
            }
        }
    }

    if std::env::var("AGENTRY_REDIS__URL").is_err() {
        if have_password {
            let pw = std::env::var("AGENTRY_REDIS_PASSWORD").unwrap_or_default();
            std::env::set_var(
                "AGENTRY_REDIS__URL",
                format!("redis://:{pw}@127.0.0.1:6380"),
            );
        } else {
            missing.push(
                "AGENTRY_REDIS__URL — would be built as \
                 `redis://:<password>@127.0.0.1:6380` once \
                 AGENTRY_REDIS_PASSWORD is resolvable"
                    .to_string(),
            );
        }
    }

    if std::env::var("AGENTRY_SIGNING__KEY_PATH").is_err() {
        std::env::set_var("AGENTRY_SIGNING__KEY_PATH", &signing_key_path);
        if !signing_key_path.exists() {
            missing.push(format!(
                "AGENTRY_SIGNING__KEY_PATH — expected at {} \
                 (this file is normally created by `orchestrator key-gen`)",
                signing_key_path.display(),
            ));
        }
    }

    if !missing.is_empty() {
        eprintln!("// captain dispatch: missing canonical configuration:");
        for m in &missing {
            eprintln!("//   - {m}");
        }
        return Err(anyhow!(
            "captain dispatch: {} missing configuration item(s); \
             see the operator-facing block above",
            missing.len()
        ));
    }

    Ok(())
}
