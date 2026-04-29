//! Seed-from-directory loader for role JSON catalogs.
//!
//! Reads every `*.json` file in a given directory, deserializes it as
//! `AgentRole` under strict serde (unknown fields rejected), and persists
//! each via `redis_io::save_role`. The directory is optional — if it does
//! not exist the loader returns an empty `Vec` so substrates that don't ship
//! a role catalog can still call seed unconditionally.

use crate::{redis_io, Error, Result};
use orchestrator_types::{AgentRole, RoleName};
use redis::aio::ConnectionManager;
use std::path::Path;

/// Expand `~/` and `${HOME}` substrings in a Mount.source path against the
/// substrate-side `$HOME`. A bare `~` (no slash) is left unmodified to avoid
/// surprise expansion of unrelated tilde-prefixed paths. Only the bracketed
/// `${HOME}` form is expanded — full shell variable expansion is out of scope.
fn expand_home_in_source(source: &str, home: &str) -> String {
    let mut out = if let Some(rest) = source.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        source.to_string()
    };
    if out.contains("${HOME}") {
        out = out.replace("${HOME}", home);
    }
    out
}

/// Load every `*.json` role file in `dir` into Redis. Returns the list of
/// names registered, in the order the files were processed (sorted by file
/// name for determinism).
///
/// * If `dir` does not exist, returns `Ok(vec![])` — silent skip.
/// * If a file fails to deserialize, the error is propagated wrapped with
///   the file path in the context.
pub async fn load_roles_from_dir(
    conn: &mut ConnectionManager,
    dir: &Path,
) -> Result<Vec<RoleName>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut json_files: Vec<std::path::PathBuf> = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            json_files.push(path);
        }
    }
    json_files.sort();

    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/home/yg".into());

    let mut out: Vec<RoleName> = Vec::with_capacity(json_files.len());
    for path in json_files {
        let text = tokio::fs::read_to_string(&path).await?;
        let mut role: AgentRole = serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("failed to parse {}: {}", path.display(), e)))?;
        for mount in role.mounts.iter_mut() {
            let expanded = expand_home_in_source(&mount.source, &home);
            if expanded != mount.source {
                tracing::info!(
                    role_name = %role.name.0,
                    original = %mount.source,
                    expanded = %expanded,
                    "expanded mount source",
                );
                mount.source = expanded;
            }
        }
        redis_io::save_role(conn, &role).await?;
        tracing::info!(
            role_name = %role.name.0,
            file_path = %path.display(),
            "loaded role from JSON file",
        );
        out.push(role.name);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_types::{
        AgentRole, Mount, PackageManager, PermitScope, RoleName, SubstrateClass, ToolAllowlist,
    };
    use std::path::PathBuf;

    #[test]
    fn expands_tilde_in_mount_source() {
        let s = expand_home_in_source("~/foo/bar", "/home/agent");
        assert_eq!(s, "/home/agent/foo/bar");
    }

    #[test]
    fn expands_braced_home_in_mount_source() {
        let s = expand_home_in_source("${HOME}/foo/bar", "/home/agent");
        assert_eq!(s, "/home/agent/foo/bar");
    }

    #[test]
    fn leaves_absolute_paths_unchanged() {
        let s = expand_home_in_source("/usr/local/bin/x", "/home/agent");
        assert_eq!(s, "/usr/local/bin/x");
    }

    #[test]
    fn leaves_bare_tilde_unchanged() {
        // Just a tilde with no slash — leave as-is to avoid surprise.
        let s = expand_home_in_source("~", "/home/agent");
        assert_eq!(s, "~");
    }

    /// End-to-end-ish: build a role with a `~/...` mount, serialize it, parse
    /// it back through the same logic the loader uses, and confirm the
    /// expansion happens before the role would be persisted. Stops short of
    /// the live Redis save (covered by ignored integration tests above).
    #[test]
    fn loader_expansion_rewrites_role_mounts_before_save() {
        let mut role = AgentRole {
            name: RoleName("rdl-mount-test".into()),
            version: 1,
            model: None,
            system_prompt: None,
            image: "alpine:3.21".into(),
            substrate_class: SubstrateClass::Podman,
            package_manager: PackageManager::Apk,
            entrypoint_script: "#!/bin/sh\nexit 0\n".into(),
            exitpoint_script: None,
            binaries: vec![],
            mcp_servers: vec![],
            tool_allowlist: ToolAllowlist::default(),
            permit_scope: PermitScope::default(),
            passthru_env: vec![],
            mounts: vec![
                Mount {
                    source: "~/foo/bar".into(),
                    target: "/x/foo/bar".into(),
                    readonly: true,
                },
                Mount {
                    source: "${HOME}/baz".into(),
                    target: "/x/baz".into(),
                    readonly: false,
                },
                Mount {
                    source: "/usr/local/bin/keep".into(),
                    target: "/usr/local/bin/keep".into(),
                    readonly: true,
                },
            ],
            workspace_mount: None,
            sccache: false,
            extra_bootstrap: vec![],
        };
        let home = "/home/agent";
        for mount in role.mounts.iter_mut() {
            mount.source = expand_home_in_source(&mount.source, home);
        }
        assert_eq!(role.mounts[0].source, "/home/agent/foo/bar");
        assert_eq!(role.mounts[1].source, "/home/agent/baz");
        assert_eq!(role.mounts[2].source, "/usr/local/bin/keep");
    }

    fn test_redis_url() -> Option<String> {
        std::env::var("AGENTRY_TEST_REDIS_URL").ok()
    }

    fn slug() -> String {
        format!(
            "rdl_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    fn minimal_role(name: &str) -> AgentRole {
        AgentRole {
            name: RoleName(name.into()),
            version: 1,
            model: None,
            system_prompt: None,
            image: "alpine:3.21".into(),
            substrate_class: SubstrateClass::Podman,
            package_manager: PackageManager::Apk,
            entrypoint_script: "#!/usr/bin/env bash\nexit 0\n".into(),
            exitpoint_script: None,
            binaries: vec![],
            mcp_servers: vec![],
            tool_allowlist: ToolAllowlist::default(),
            permit_scope: PermitScope::default(),
            passthru_env: vec![],
            mounts: vec![],
            workspace_mount: None,
            sccache: false,
            extra_bootstrap: vec![],
        }
    }

    fn write_role(dir: &Path, file_name: &str, role: &AgentRole) -> PathBuf {
        let p = dir.join(file_name);
        let body = serde_json::to_string_pretty(role).expect("ser role");
        std::fs::write(&p, body).expect("write role");
        p
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL) — connection is unused on the early-return path but still required by the signature"]
    async fn loads_empty_dir_returns_empty_vec() {
        let Some(url) = test_redis_url() else { return };
        let mut conn = redis_io::connect(&url).await.expect("connect");
        let result = load_roles_from_dir(&mut conn, Path::new("/nonexistent/path"))
            .await
            .expect("non-existent dir is OK");
        assert!(result.is_empty(), "missing dir must yield empty Vec");
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn loads_two_role_jsons_returns_both_names() {
        let Some(url) = test_redis_url() else { return };
        let mut conn = redis_io::connect(&url).await.expect("connect");

        let s = slug();
        let n_a = format!("zz-rdl-a-{s}");
        let n_b = format!("zz-rdl-b-{s}");
        let dir = tempfile::tempdir().expect("tempdir");
        write_role(dir.path(), "a.json", &minimal_role(&n_a));
        write_role(dir.path(), "b.json", &minimal_role(&n_b));

        let names = load_roles_from_dir(&mut conn, dir.path())
            .await
            .expect("load");
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].0, n_a);
        assert_eq!(names[1].0, n_b);

        // Round-trip: each role must now be retrievable by `fetch_role`.
        let fetched_a = redis_io::fetch_role(&mut conn, &RoleName(n_a.clone()), 1)
            .await
            .expect("fetch a");
        assert_eq!(fetched_a.name.0, n_a);
        let fetched_b = redis_io::fetch_role(&mut conn, &RoleName(n_b.clone()), 1)
            .await
            .expect("fetch b");
        assert_eq!(fetched_b.name.0, n_b);

        // Cleanup.
        use redis::AsyncCommands;
        let _: () = conn
            .del::<_, ()>(format!("agentry:role:{n_a}:v1"))
            .await
            .expect("cleanup a");
        let _: () = conn
            .del::<_, ()>(format!("agentry:role:{n_b}:v1"))
            .await
            .expect("cleanup b");
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn loads_malformed_json_returns_err() {
        let Some(url) = test_redis_url() else { return };
        let mut conn = redis_io::connect(&url).await.expect("connect");

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("broken.json"), b"{ not valid json").expect("write broken");
        let r = load_roles_from_dir(&mut conn, dir.path()).await;
        assert!(r.is_err(), "malformed JSON must propagate as Err");
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn loads_skips_non_json_files() {
        let Some(url) = test_redis_url() else { return };
        let mut conn = redis_io::connect(&url).await.expect("connect");

        let s = slug();
        let role_name = format!("zz-rdl-only-{s}");
        let dir = tempfile::tempdir().expect("tempdir");
        write_role(dir.path(), "role.json", &minimal_role(&role_name));
        std::fs::write(dir.path().join("README.md"), b"# notes\n").expect("write readme");

        let names = load_roles_from_dir(&mut conn, dir.path())
            .await
            .expect("load");
        assert_eq!(names.len(), 1, "non-JSON files must be ignored");
        assert_eq!(names[0].0, role_name);

        use redis::AsyncCommands;
        let _: () = conn
            .del::<_, ()>(format!("agentry:role:{role_name}:v1"))
            .await
            .expect("cleanup");
    }

    /// Brief 190b of #182: the seed/roles directory at the workspace root
    /// must contain JSON role files that deserialize cleanly as `AgentRole`.
    /// Pure parse check — no Redis required, no `load_roles_from_dir` call.
    /// CI's `orchestrator seed` does the live end-to-end load.
    #[test]
    fn loads_seed_roles_dir_at_workspace_root() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root from CARGO_MANIFEST_DIR");
        let seed_roles = workspace_root.join("seed").join("roles");

        let commit_json = seed_roles.join("git-op-commit-v1.json");
        let push_json = seed_roles.join("git-op-push-v1.json");
        assert!(
            commit_json.exists(),
            "git-op-commit-v1.json missing at {}",
            commit_json.display()
        );
        assert!(
            push_json.exists(),
            "git-op-push-v1.json missing at {}",
            push_json.display()
        );

        let commit_text = std::fs::read_to_string(&commit_json).expect("read commit JSON");
        let commit_role: AgentRole =
            serde_json::from_str(&commit_text).expect("git-op-commit-v1.json deserialize");
        assert_eq!(commit_role.name.0, "git-op-commit");
        assert_eq!(commit_role.version, 1);
        assert!(
            commit_role
                .entrypoint_script
                .contains("exec /usr/local/bin/git-op-commit"),
            "commit role entrypoint must exec the git-op-commit binary"
        );

        let push_text = std::fs::read_to_string(&push_json).expect("read push JSON");
        let push_role: AgentRole =
            serde_json::from_str(&push_text).expect("git-op-push-v1.json deserialize");
        assert_eq!(push_role.name.0, "git-op-push");
        assert_eq!(push_role.version, 1);
        assert!(
            push_role
                .entrypoint_script
                .contains("exec /usr/local/bin/git-op-push"),
            "push role entrypoint must exec the git-op-push binary"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn loads_in_alphabetical_order() {
        let Some(url) = test_redis_url() else { return };
        let mut conn = redis_io::connect(&url).await.expect("connect");

        let s = slug();
        let n_a = format!("zz-rdl-ord-a-{s}");
        let n_b = format!("zz-rdl-ord-b-{s}");
        let n_c = format!("zz-rdl-ord-c-{s}");
        let dir = tempfile::tempdir().expect("tempdir");
        // Write in scrambled order — sort must be by file name, not creation.
        write_role(dir.path(), "c.json", &minimal_role(&n_c));
        write_role(dir.path(), "a.json", &minimal_role(&n_a));
        write_role(dir.path(), "b.json", &minimal_role(&n_b));

        let names = load_roles_from_dir(&mut conn, dir.path())
            .await
            .expect("load");
        let collected: Vec<String> = names.iter().map(|n| n.0.clone()).collect();
        assert_eq!(collected, vec![n_a.clone(), n_b.clone(), n_c.clone()]);

        use redis::AsyncCommands;
        for n in [n_a, n_b, n_c] {
            let _: () = conn
                .del::<_, ()>(format!("agentry:role:{n}:v1"))
                .await
                .expect("cleanup");
        }
    }
}
