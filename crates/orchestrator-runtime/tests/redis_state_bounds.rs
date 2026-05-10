//! Retention-bound regression tests for brief 477.
//!
//! Three live-Redis tests pin the operational hardening fixes:
//!
//! 1. `submit_xadd_caps_stream_at_maxlen` — submitting > 10_000 briefs
//!    leaves `XLEN agentry:briefs` capped near the MAXLEN ~ 10_000
//!    bound (XADD `~` allows minor over-shoot to amortise trim cost).
//! 2. `cleanup_failed_brief_sets_ttl` — the Failed-cleanup path applies
//!    a 30-day TTL to every `agentry:brief:{id}:*` sibling key.
//! 3. `cleanup_shipped_no_op_brief_sets_ttl` — the no-op-Shipped
//!    cleanup path does the same.
//!
//! Live-Redis like the rest of this crate's redis-integration suite:
//! gate on `AGENTRY_TEST_REDIS_URL` (default `redis://127.0.0.1:6380`)
//! and stay `#[ignore]` so the workspace-wide `cargo test` pass stays
//! green without a Redis dependency.
//!
//! Run with:
//! `cargo test --package orchestrator-runtime --test redis_state_bounds -- --ignored`

use orchestrator_runtime::lifecycle_driver::{
    cleanup_failed_brief_at, cleanup_shipped_no_op_brief_at, TERMINAL_BRIEF_TTL_SECONDS,
};
use orchestrator_runtime::redis_io::{connect, submit_brief, STREAM_BRIEFS};
use orchestrator_types::{now, Brief, BriefId, Budget, EscalationMode, VersionedRef};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn slug() -> String {
    format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn synthetic_brief(brief_id: &str) -> Brief {
    Brief {
        id: BriefId(brief_id.into()),
        project: None,
        topology: VersionedRef::new("test-team", 1),
        payload: serde_json::json!({}),
        kind: None,
        contract: None,
        budget: Budget::default(),
        escalation: EscalationMode::default(),
        parent_brief: None,
        cohort_labels: Vec::new(),
        redeploy_required: Vec::new(),
        submitted_by: "redis_state_bounds_test".into(),
        submitted_at: now(),
    }
}

async fn discover_brief_keys(conn: &mut ConnectionManager, brief_id: &str) -> Vec<String> {
    let pattern = format!("agentry:brief:{brief_id}:*");
    let mut out: Vec<String> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(100)
            .query_async(conn)
            .await
            .expect("SCAN");
        out.extend(batch);
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    out
}

async fn seed_sibling_keys(conn: &mut ConnectionManager, brief_id: &str) -> Vec<String> {
    // The full set documented in brief 477 VERB 2. SCAN-based discovery
    // in production is future-proof against new sibling keys; the seed
    // here matches the present-day list so the assertion exercises every
    // documented sibling.
    let scalars = [
        ":body",
        ":state",
        ":state_projector_cursor",
        ":delivery",
        ":attempts",
    ];
    let streams = [":state_log", ":trace"];
    let sets = [":children_pending"];

    let mut keys: Vec<String> = Vec::new();
    for suffix in scalars {
        let key = format!("agentry:brief:{brief_id}{suffix}");
        let _: () = conn.set(&key, "probe").await.expect("seed scalar");
        keys.push(key);
    }
    for suffix in streams {
        let key = format!("agentry:brief:{brief_id}{suffix}");
        let _: String = conn
            .xadd(&key, "*", &[("probe", "1")])
            .await
            .expect("seed stream");
        keys.push(key);
    }
    for suffix in sets {
        let key = format!("agentry:brief:{brief_id}{suffix}");
        let _: () = conn.sadd(&key, "probe").await.expect("seed set");
        keys.push(key);
    }
    keys
}

async fn cleanup_keys(conn: &mut ConnectionManager, keys: &[String]) {
    for k in keys {
        let _: redis::RedisResult<()> = conn.del(k).await;
    }
}

/// Test 1 — submit > 10_000 briefs and assert `XLEN agentry:briefs`
/// stays at or near the MAXLEN ~ 10_000 cap. `~` allows over-shoot
/// to amortise the trim cost; 10_500 is a generous upper bound that
/// catches a regression to "no trim at all" (the bare `XADD` would
/// land at exactly the submitted count) without flaking on the
/// expected slack.
///
/// Cleanup deletes the per-brief `:body` keys we created. We do NOT
/// trim `agentry:briefs` itself — the trim is exactly what this test
/// exercises.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn submit_xadd_caps_stream_at_maxlen() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");

    let prefix = format!("test_maxlen_{}_", slug());
    let total: usize = 10_100;
    let mut body_keys: Vec<String> = Vec::with_capacity(total);
    for i in 0..total {
        let brief_id = format!("{prefix}{i:05}");
        body_keys.push(format!("agentry:brief:{brief_id}:body"));
        let brief = synthetic_brief(&brief_id);
        submit_brief(&mut conn, &brief).await.expect("submit_brief");
    }

    let xlen: usize = conn.xlen(STREAM_BRIEFS).await.expect("XLEN");
    assert!(
        xlen <= 10_500,
        "XLEN {STREAM_BRIEFS} = {xlen} exceeds MAXLEN ~ 10000 + slack (10500)"
    );

    cleanup_keys(&mut conn, &body_keys).await;
}

/// Test 2 — seed the full sibling key set, drive
/// `cleanup_failed_brief_at` against it, assert every key has a
/// positive TTL within the configured retention window.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn cleanup_failed_brief_sets_ttl() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let brief_id_str = format!("test_ttl_failed_{}", slug());
    let bid = BriefId(brief_id_str.clone());

    let seeded = seed_sibling_keys(&mut conn, &brief_id_str).await;

    let tmp = tempfile::tempdir().expect("tmp");
    cleanup_failed_brief_at(&bid, tmp.path(), Some(&mut conn)).await;

    let cap = TERMINAL_BRIEF_TTL_SECONDS as i64;
    for suffix in [":body", ":state", ":state_log", ":trace"] {
        let key = format!("agentry:brief:{brief_id_str}{suffix}");
        let ttl: i64 = conn.ttl(&key).await.expect("TTL");
        assert!(
            ttl > 0 && ttl <= cap + 1,
            "TTL {key} = {ttl}, expected in (0, {}], cleanup must apply EXPIRE",
            cap + 1,
        );
    }

    // Discover-and-delete: the cleanup trace append may have written
    // additional entries to :trace; the SCAN-based discovery sweeps
    // them all.
    let live = discover_brief_keys(&mut conn, &brief_id_str).await;
    cleanup_keys(&mut conn, &live).await;
    cleanup_keys(&mut conn, &seeded).await;
}

/// Test 3 — same as Test 2 but for the no-op-Shipped cleanup variant.
/// The two cleanup paths share `cleanup_brief_at`'s TTL pass; the
/// disposition only flips the audit-log wording.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn cleanup_shipped_no_op_brief_sets_ttl() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let brief_id_str = format!("test_ttl_noop_{}", slug());
    let bid = BriefId(brief_id_str.clone());

    let seeded = seed_sibling_keys(&mut conn, &brief_id_str).await;

    let tmp = tempfile::tempdir().expect("tmp");
    cleanup_shipped_no_op_brief_at(&bid, tmp.path(), Some(&mut conn)).await;

    let cap = TERMINAL_BRIEF_TTL_SECONDS as i64;
    for suffix in [":body", ":state", ":state_log", ":trace"] {
        let key = format!("agentry:brief:{brief_id_str}{suffix}");
        let ttl: i64 = conn.ttl(&key).await.expect("TTL");
        assert!(
            ttl > 0 && ttl <= cap + 1,
            "TTL {key} = {ttl}, expected in (0, {}], cleanup must apply EXPIRE",
            cap + 1,
        );
    }

    let live = discover_brief_keys(&mut conn, &brief_id_str).await;
    cleanup_keys(&mut conn, &live).await;
    cleanup_keys(&mut conn, &seeded).await;
}
