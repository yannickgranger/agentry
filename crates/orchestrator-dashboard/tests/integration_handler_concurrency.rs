//! Concurrent-load reproduction test for issue #169 step (b).
//!
//! Spawns 32 parallel `fetch_recent_verdicts` calls through a single shared
//! `DashboardStore` (which currently funnels every command through one
//! `ConnectionManager`) and asserts p99 elapsed < 50 ms.
//!
//! Today this is expected to pass: each `XREVRANGE` returns well under
//! 1 ms, and the per-handler instrumentation shipped in step (a) gives
//! production logs a way to catch a real-world slow case. This test then
//! becomes the regression baseline that gates the eventual connection-pool
//! refactor in step (c).
//!
//! Gated on `AGENTRY_TEST_REDIS_URL` so the workspace-wide `cargo test`
//! pass stays green without a Redis dependency. The skip pattern matches
//! `integration_pipe_sharing.rs`: read the env var, early-return when
//! unset.
//!
//! Multi-thread tokio flavor (`worker_threads = 4`) matches the dashboard
//! crate's other live-Redis test.

use std::time::{Duration, Instant};

use orchestrator_dashboard::store::DashboardStore;

const N_TASKS: usize = 32;
const FETCH_COUNT: usize = 8;
const P99_THRESHOLD: Duration = Duration::from_millis(50);

fn redis_url_or_skip() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p99_under_50ms_for_32_parallel_fetch_recent_verdicts() {
    let Some(url) = redis_url_or_skip() else {
        eprintln!("AGENTRY_TEST_REDIS_URL not set — skipping handler concurrency repro");
        return;
    };

    let store = DashboardStore::new(&url)
        .await
        .expect("DashboardStore::new");

    let mut handles = Vec::with_capacity(N_TASKS);
    for _ in 0..N_TASKS {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            let started = Instant::now();
            let result = s.fetch_recent_verdicts(FETCH_COUNT).await;
            (started.elapsed(), result.is_ok())
        }));
    }

    let mut latencies: Vec<Duration> = Vec::with_capacity(N_TASKS);
    let mut all_ok = true;
    for h in handles {
        let (elapsed, ok) = h.await.expect("task join");
        latencies.push(elapsed);
        if !ok {
            all_ok = false;
        }
    }
    assert!(all_ok, "at least one fetch_recent_verdicts call failed");

    latencies.sort();
    let p99_idx = ((N_TASKS * 99) / 100) - 1;
    let p99 = latencies[p99_idx];
    let p99_ms = p99.as_secs_f64() * 1_000.0;

    assert!(
        p99 < P99_THRESHOLD,
        "p99 latency {p99_ms:.3} ms exceeded 50 ms threshold across {N_TASKS} \
         parallel fetch_recent_verdicts calls — issue #169 contention regression"
    );
}
