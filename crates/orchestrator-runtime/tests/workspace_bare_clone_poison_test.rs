//! Asserts the `unwrap_or_else(|poison| poison.into_inner())` recovery
//! pattern on the `bare_clone_locks` static `Mutex<HashMap>` in
//! `crate::workspace`: a prior panic that poisons the lock must NOT
//! cascade into a panic on every subsequent `lock_for_bare` call —
//! which today is on the hot path for every workspace allocation
//! against a (repo_url, base_branch).
//!
//! Mirrors `orchestrator-infra/tests/runtime_registry_poison_test.rs`
//! verbatim in shape. The test reaches the otherwise-private statics
//! via `__bare_clone_locks_for_test` / `__lock_for_bare_for_test`
//! (see `workspace.rs`); inline `#[cfg(test)] mod tests` in src/ is
//! banned by `arch-ban-inline-cfg-test-in-src`.

use std::panic;
use std::path::Path;
use std::thread;

use orchestrator_runtime::workspace::{__bare_clone_locks_for_test, __lock_for_bare_for_test};

#[test]
fn bare_clone_lock_recovers_from_poison() {
    let handle = thread::spawn(|| {
        let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _g = __bare_clone_locks_for_test()
                .lock()
                .expect("acquire bare-clone lock for poisoning");
            panic!("intentional panic to poison the bare-clone lock");
        }));
    });
    handle.join().expect("poison thread joinable");

    assert!(
        __bare_clone_locks_for_test().is_poisoned(),
        "bare-clone lock should be poisoned after the panicking thread joined"
    );

    let arc = __lock_for_bare_for_test(Path::new("/tmp/agentry-bare-clone-poison-recovery"));
    // The returned Arc<TokioMutex> must be usable. TokioMutex has no
    // poisoning semantics, so a successful try_lock is the right shape.
    let _hold = arc.try_lock().expect("returned TokioMutex is lockable");
}
