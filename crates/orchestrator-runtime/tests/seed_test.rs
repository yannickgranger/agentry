//! Public-surface tests for the seed module.
//!
//! `seed.rs` is dominated by private bash-script string constants and
//! private `build_*_role` helpers. The migration recipe forbids promoting
//! their visibility, so the inline assertions over `BASH_PRELUDE`,
//! `*_AGENTRY_SCRIPT`, and `build_*_role` outputs are dropped — those
//! invariants are exercised end-to-end by `orchestrator seed` against a
//! live Redis. What survives here is the public on-disk role-JSON contract
//! that the role_dir_loader exercises at every seed.

use orchestrator_types::AgentRole;
use std::path::PathBuf;

/// Issue #175: roles whose entrypoint exec's a host-built runner binary
/// must run on an image whose glibc is at least the host build's
/// compile-target. `debian:trixie-slim` (glibc 2.41) satisfies a Fedora 43
/// (glibc 2.42) host build; the prior `bookworm-slim` (glibc 2.36) failed
/// dynamic-linker resolution before `main()` and silently exited.
#[test]
fn runner_host_roles_use_glibc_compatible_image() {
    const RUNNER_HOST_IMAGE: &str = "docker.io/library/debian:trixie-slim";
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let seed_roles = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root from CARGO_MANIFEST_DIR")
        .join("seed/roles");
    let files = [
        "reviewer-claude-agentry-v1.json",
        "ac-verifier-claude-agentry-v1.json",
        "ac-verifier-gemini-agentry-v1.json",
        "ac-verifier-grok-agentry-v1.json",
    ];
    for file in files {
        let path = seed_roles.join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let role: AgentRole =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        assert_eq!(
            role.image, RUNNER_HOST_IMAGE,
            "role '{}' (from {file}) must use {RUNNER_HOST_IMAGE} — see #175",
            role.name
        );
        assert!(
            role.entrypoint_script.contains("exec /usr/local/bin/")
                && role.entrypoint_script.contains("-runner"),
            "role '{}' (from {file}) is expected to exec a host-built runner binary",
            role.name
        );
    }
}
