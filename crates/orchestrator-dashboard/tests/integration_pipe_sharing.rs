//! Reproduction harness for issue #167:
//! `DashboardStore`'s tail loop holds the shared `ConnectionManager` pipe
//! during `XREAD ... BLOCK 5000`, serialising every other command issued
//! through any clone of the same ConnectionManager.
//!
//! This test:
//! 1. Builds a `DashboardStore` against the live agentry Redis.
//! 2. Subscribes to verdicts — that spawns a tail loop which parks on
//!    `XREAD BLOCK <block_ms>` immediately.
//! 3. Calls `fetch_recent_verdicts` (a single `XREVRANGE`) and times it.
//!
//! Bug claim: the second call should take ~`block_ms` because the pipe is
//! shared. With `block_ms = 800`, a healthy implementation returns in
//! < 100 ms; the buggy one waits ~800 ms.
//!
//! Gated on `AGENTRY_TEST_REDIS_URL` so CI without redis just skips.

use std::time::{Duration, Instant};

use orchestrator_dashboard::store::DashboardStore;

fn redis_url_or_skip() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_blocked_behind_tail_loop_xread() {
    let Some(url) = redis_url_or_skip() else {
        eprintln!("AGENTRY_TEST_REDIS_URL not set — skipping pipe-sharing repro");
        return;
    };

    // Use a short BLOCK window so the test fails fast and CI is bearable.
    // Real prod uses 5000 ms; 800 ms is enough to make the bug obvious
    // (>>100 ms acceptance threshold) without making the test sluggish.
    let block_ms: u64 = 800;
    let eviction_grace = Duration::from_secs(30);

    let store = DashboardStore::new_with(&url, block_ms, eviction_grace)
        .await
        .expect("DashboardStore::new_with");

    // Subscribe — this spawns a tail loop. The loop immediately calls
    // `xread BLOCK block_ms ...` against agentry:verdicts. Hold the
    // receiver alive so the tail isn't reaped.
    let _rx = store.subscribe_verdicts();

    // Give the spawned task a moment to actually issue the XREAD.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now race a normal command. Expectation (healthy): well under 100 ms.
    // Actual (buggy): waits ~block_ms because the pipe is parked.
    let start = Instant::now();
    let _ = store
        .fetch_recent_verdicts(20)
        .await
        .expect("fetch verdicts");
    let elapsed = start.elapsed();

    // Slack: 200 ms is generous on a developer host; agentry CI is similar.
    assert!(
        elapsed < Duration::from_millis(200),
        "regression: fetch_recent_verdicts took {elapsed:?} while a tail \
         loop was parked on XREAD BLOCK {block_ms}ms — pipe-sharing bug \
         (issue #167)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_blocked_behind_tail_loop_xread() {
    let Some(url) = redis_url_or_skip() else {
        eprintln!("AGENTRY_TEST_REDIS_URL not set — skipping list pipe-sharing repro");
        return;
    };

    let block_ms: u64 = 800;
    let eviction_grace = Duration::from_secs(30);

    let store = DashboardStore::new_with(&url, block_ms, eviction_grace)
        .await
        .expect("DashboardStore::new_with");

    let _rx = store.subscribe_verdicts();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = Instant::now();
    let _: Vec<(String, serde_json::Value)> = store.list("role").await.expect("list roles");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(200),
        "regression: list(\"role\") took {elapsed:?} while a tail loop was \
         parked on XREAD BLOCK {block_ms}ms — pipe-sharing bug (issue #167)"
    );
}
