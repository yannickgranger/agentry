//! Slice I/1d end-to-end: the seeded `coder-claude-agentry-v1` role
//! references the seeded `quality-fast-v1` tool pack and resolves cleanly
//! through `spawner::resolve_role_with_packs`.
//!
//! Live-Redis test gated on `AGENTRY_TEST_REDIS_URL` and `#[ignore]`'d so
//! the workspace `cargo test` pass stays green without a Redis dependency
//! — same convention as `tests/role_pack_resolve_test.rs` and
//! `tests/tool_pack_seed_test.rs`.

use orchestrator_runtime::redis_io;
use orchestrator_runtime::spawner::resolve_role_with_packs;
use orchestrator_types::{AgentRole, ToolPack};
use std::path::PathBuf;

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root from CARGO_MANIFEST_DIR")
}

fn read_seed_role() -> AgentRole {
    let path = workspace_root()
        .join("seed")
        .join("roles")
        .join("coder-claude-agentry-v1.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("deserialize coder-claude-agentry-v1.json: {e}"))
}

fn read_seed_pack() -> ToolPack {
    let path = workspace_root()
        .join("seed")
        .join("packs")
        .join("quality-fast.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("deserialize quality-fast.json: {e}"))
}

/// Pure (no-Redis) sanity: the seeded role JSON declares `tool_packs:
/// ["quality-fast"]` and the seeded pack JSON parses as a `ToolPack`. Proves
/// the on-disk wiring without needing Redis.
#[test]
fn seeded_role_references_quality_fast_pack_on_disk() {
    let role = read_seed_role();
    assert_eq!(role.name.0, "coder-claude-agentry");
    assert_eq!(
        role.tool_packs,
        vec!["quality-fast".to_string()],
        "seeded role must reference the quality-fast pack",
    );

    let pack = read_seed_pack();
    assert_eq!(pack.name, "quality-fast");
    assert_eq!(pack.version, 1);
    assert!(
        pack.system_prompt_fragment
            .as_deref()
            .is_some_and(|s| s.contains("You have quality-fast on PATH")),
        "seeded pack must contribute the quality-fast usage fragment",
    );
}

/// End-to-end: seed the on-disk role and pack into Redis, resolve, and
/// inspect the merged effective role.
///
/// - `binaries` is the role's apt-install list. `quality-fast` is a
///   host-mounted binary, NOT a Debian package, so neither the role nor
///   the pack puts it in `binaries` (otherwise apt-get install fails with
///   exit 100). The pack's empty `binaries` is intentional.
/// - `allowed_tools` already declares `Bash(quality-fast:*)` on the role;
///   the pack contributes the same pattern. The merge dedups so the
///   resolved role lists the pattern exactly once.
/// - `system_prompt` is `null` on the role; the pack contributes a
///   genuinely new fragment that the merge folds in. Asserting on the
///   substring proves the merge actually flowed through.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn coder_claude_agentry_resolves_quality_fast_pack() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = redis_io::connect(&url).await.expect("connect");

    let role = read_seed_role();
    let pack = read_seed_pack();

    redis_io::save_role(&mut conn, &role)
        .await
        .expect("seed role");
    redis_io::seed_pack(&mut conn, &pack)
        .await
        .expect("seed pack");

    let resolved = resolve_role_with_packs(&role, &mut conn)
        .await
        .expect("resolve");

    assert!(
        !resolved
            .binaries
            .iter()
            .any(|b| b.as_str() == "quality-fast"),
        "quality-fast is host-mounted, not apt-installed; binaries must NOT contain it; got {:?}",
        resolved.binaries,
    );

    let allowed = resolved
        .allowed_tools
        .as_ref()
        .expect("resolved role must keep its allowed_tools");
    let tool_count = allowed
        .0
        .iter()
        .filter(|t| t.as_str() == "Bash(quality-fast:*)")
        .count();
    assert_eq!(
        tool_count, 1,
        "allowed_tools must contain `Bash(quality-fast:*)` exactly once after dedup; got {:?}",
        allowed.0,
    );

    let prompt = resolved
        .system_prompt
        .as_deref()
        .expect("merge must have set system_prompt from the pack fragment");
    assert!(
        prompt.contains("You have quality-fast on PATH"),
        "system_prompt must include the pack's quality-fast usage fragment; got {prompt:?}",
    );

    use redis::AsyncCommands;
    let _: () = conn
        .del::<_, ()>(format!("agentry:role:{}:v{}", role.name.0, role.version))
        .await
        .expect("cleanup role");
    let _: () = conn
        .del::<_, ()>(format!("agentry:tool_pack:{}:v{}", pack.name, pack.version))
        .await
        .expect("cleanup pack");
}
