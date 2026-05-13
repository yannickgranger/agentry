//! Asserts the `unwrap_or_else(|poison| poison.into_inner())` recovery
//! pattern on `runtime_registry`'s static RwLock: a prior panic that
//! poisons the lock must NOT cascade into a panic of every subsequent
//! `register_running` / `workspace_path` call.
//!
//! The test reaches the static lock via the `__registry_for_test` hook
//! (see `runtime_registry.rs`); the lock is otherwise private and no
//! public entry point holds a guard across user code, so deterministic
//! poisoning is not reachable through the regular API.

use std::panic;
use std::path::PathBuf;
use std::thread;

use orchestrator_infra::runtime_registry::{
    __registry_for_test, register_running_with_guard, workspace_path, ContainerHandle,
};
use orchestrator_types::BriefId;

#[test]
fn workspace_path_and_register_recover_from_poisoned_lock() {
    let pre_id = BriefId("brf_poison_recovery_pre".to_string());
    let pre_workspace = PathBuf::from("/tmp/poison-recovery-pre");
    let _pre_guard = register_running_with_guard(
        pre_id.clone(),
        ContainerHandle {
            container_name: "container-poison-pre".to_string(),
            workspace_path: Some(pre_workspace.clone()),
        },
    );

    let handle = thread::spawn(|| {
        let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _w = __registry_for_test()
                .write()
                .expect("acquire write lock for poisoning");
            panic!("intentional panic to poison the registry write-lock");
        }));
    });
    handle.join().expect("poison thread joinable");

    assert!(
        __registry_for_test().is_poisoned(),
        "lock should be poisoned"
    );

    assert_eq!(workspace_path(&pre_id), Some(pre_workspace));

    let post_id = BriefId("brf_poison_recovery_post".to_string());
    let post_workspace = PathBuf::from("/tmp/poison-recovery-post");
    let _post_guard = register_running_with_guard(
        post_id.clone(),
        ContainerHandle {
            container_name: "container-poison-post".to_string(),
            workspace_path: Some(post_workspace.clone()),
        },
    );
    assert_eq!(workspace_path(&post_id), Some(post_workspace));
}
