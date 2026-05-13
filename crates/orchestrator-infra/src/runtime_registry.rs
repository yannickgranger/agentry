//! Process-wide registry of running role containers, keyed by `BriefId`.
//!
//! Lives in infra (not runtime) so the dashboard can address running
//! containers without dragging in the daemon monolith. The runtime's
//! `spawner` module writes via [`RegistrationGuard`]; the dashboard's
//! `briefs` routes read via [`workspace_path`] and [`kill`].
//!
//! CRITICAL: every insert-on-spawn is paired with a `Drop`-fired removal via
//! `RegistrationGuard`. A manual `unregister_running` call positioned after
//! `child.wait()` would leak the entry on any `?`-bubbled error between
//! spawn and wait.

use crate::{Error, Result};
use orchestrator_types::BriefId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};
use tracing;

#[derive(Debug, Clone)]
pub struct ContainerHandle {
    pub container_name: String,
    pub workspace_path: Option<PathBuf>,
}

fn registry() -> &'static RwLock<HashMap<BriefId, ContainerHandle>> {
    static R: OnceLock<RwLock<HashMap<BriefId, ContainerHandle>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(HashMap::new()))
}

fn register_running(brief_id: &BriefId, handle: ContainerHandle) {
    let mut g = registry().write().unwrap_or_else(|poison| {
        tracing::warn!(
            lock = "runtime_registry",
            "recovering from poisoned write-lock"
        );
        poison.into_inner()
    });
    g.insert(brief_id.clone(), handle);
}

fn unregister_running(brief_id: &BriefId) {
    let mut g = registry().write().unwrap_or_else(|poison| {
        tracing::warn!(
            lock = "runtime_registry",
            "recovering from poisoned write-lock"
        );
        poison.into_inner()
    });
    g.remove(brief_id);
}

/// RAII guard: registers the container on construction, removes the entry
/// on `Drop`. Holding the guard across the spawn-to-wait window guarantees
/// the registry never leaks an entry, even when an early `?` returns out of
/// the spawner.
struct RegistrationGuard {
    brief_id: BriefId,
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        unregister_running(&self.brief_id);
    }
}

/// Register `handle` under `brief_id` and return an opaque RAII guard.
/// Drop the guard to remove the entry from the registry.
#[must_use]
pub fn register_running_with_guard(brief_id: BriefId, handle: ContainerHandle) -> impl Drop {
    register_running(&brief_id, handle);
    RegistrationGuard { brief_id }
}

/// SIGTERM the running container associated with `brief_id`, returning
/// `Ok(())` on signaled, `Error::NotFound` if no container is registered, or
/// a Podman error if the kill itself fails. The container's exitpoint
/// script (when configured) runs.
pub async fn kill(brief_id: &BriefId) -> Result<()> {
    let name = {
        let g = registry().read().unwrap_or_else(|poison| {
            tracing::warn!(
                lock = "runtime_registry",
                "recovering from poisoned read-lock"
            );
            poison.into_inner()
        });
        g.get(brief_id).map(|h| h.container_name.clone())
    };
    let name = name.ok_or_else(|| Error::NotFound {
        kind: "running container",
        key: brief_id.0.clone(),
    })?;
    let out = tokio::process::Command::new("podman")
        .args(["kill", "--signal", "SIGTERM", &name])
        .output()
        .await
        .map_err(|e| Error::Podman(format!("kill {name}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Podman(format!(
            "podman kill {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Look up the host workspace path of a brief's running container.
/// Returns `None` if the brief has no live container, or if the container
/// runs without a workspace mount.
#[must_use]
pub fn workspace_path(brief_id: &BriefId) -> Option<PathBuf> {
    let g = registry().read().unwrap_or_else(|poison| {
        tracing::warn!(
            lock = "runtime_registry",
            "recovering from poisoned read-lock"
        );
        poison.into_inner()
    });
    g.get(brief_id).and_then(|h| h.workspace_path.clone())
}

// Test-only accessor so `tests/runtime_registry_poison_test.rs` can drive the
// poisoning deterministically. The static lock is private by design (callers
// must go through `register_running_with_guard` / `workspace_path` /
// `kill`); this hook exists solely so the recovery contract can be
// black-box verified from an integration test, since inline `#[cfg(test)]`
// in `src/` is banned by `arch-ban-inline-cfg-test-in-src`.
#[doc(hidden)]
pub fn __registry_for_test() -> &'static RwLock<HashMap<BriefId, ContainerHandle>> {
    registry()
}
