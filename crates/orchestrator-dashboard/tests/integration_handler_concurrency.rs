//! Concurrent-load reproduction for #169 step (b). Spawns 32 parallel
//! `fetch_recent_verdicts` calls against a live test Redis, asserts p99 < 50ms.
//!
//! Today (single shared ConnectionManager): expected to pass with all calls
//! completing in <1 ms each. Used as a regression baseline. When step (c)
//! introduces a connection pool, this test stays as a regression guard.
//!
//! Marked `#[ignore]` so `cargo test --workspace` does not fail in
//! environments without Redis. Opt in via `cargo test --workspace --
//! --ignored` (or `--include-ignored`). The Redis URL is sourced from
//! `AGENTRY_TEST_REDIS_URL` (default `redis://127.0.0.1:6380`), matching the
//! convention used by the other dashboard integration tests
//! (`integration_pipe_sharing.rs` and the `store::tests` module). The dev
//! Redis password file pattern (`/var/home/yg/.config/agentry/redis.password`)
//! is used by callers that build the URL — this test only consumes the
//! already-resolved URL.

use std::time::{Duration, Instant};

use futures::future::join_all;
use orchestrator_dashboard::store::DashboardStore;
use redis::AsyncCommands;

const N_TASKS: usize = 32;
const SEEDED: usize = 10;
const FETCH_COUNT: usize = 10;
const P99_THRESHOLD: Duration = Duration::from_millis(50);
const VERDICTS_STREAM: &str = "agentry:verdicts";

fn redis_url() -> String {
    std::env::var("AGENTRY_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".into())
}

async fn seed_verdicts(conn: &mut redis::aio::ConnectionManager) -> Vec<String> {
    let mut ids = Vec::with_capacity(SEEDED);
    for i in 0..SEEDED {
        let body = format!(r#"{{"test":"brf_work_169_b","i":{i},"outcome":"complete"}}"#);
        let id: String = conn
            .xadd(VERDICTS_STREAM, "*", &[("verdict", body.as_str())])
            .await
            .expect("xadd seed verdict");
        ids.push(id);
    }
    ids
}

async fn cleanup_seeded(conn: &mut redis::aio::ConnectionManager, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let _: Result<i64, _> = conn.xdel(VERDICTS_STREAM, ids).await;
}

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    let n = sorted.len();
    let idx = ((n * p) / 100).min(n - 1);
    sorted[idx]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn p99_under_50ms_for_32_parallel_fetch_recent_verdicts() {
    let url = redis_url();
    let store = DashboardStore::new(&url)
        .await
        .expect("DashboardStore::new");

    let client = redis::Client::open(url.as_str()).expect("redis client open");
    let mut seeder = redis::aio::ConnectionManager::new(client)
        .await
        .expect("seeder ConnectionManager");

    let seeded = seed_verdicts(&mut seeder).await;

    let mut handles = Vec::with_capacity(N_TASKS);
    for _ in 0..N_TASKS {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            let started = Instant::now();
            let result = s.fetch_recent_verdicts(FETCH_COUNT).await;
            (started.elapsed(), result.is_ok(), result.map(|v| v.len()))
        }));
    }

    let mut latencies: Vec<Duration> = Vec::with_capacity(N_TASKS);
    let mut all_ok = true;
    let mut max_returned = 0usize;
    for joined in join_all(handles).await {
        let (elapsed, ok, len) = joined.expect("task join");
        latencies.push(elapsed);
        if !ok {
            all_ok = false;
        }
        if let Ok(n) = len {
            if n > max_returned {
                max_returned = n;
            }
        }
    }

    cleanup_seeded(&mut seeder, &seeded).await;

    assert!(all_ok, "at least one fetch_recent_verdicts call failed");
    assert!(
        max_returned > 0,
        "expected at least one fetch to return seeded verdicts (max_returned={max_returned})"
    );

    latencies.sort();
    let p50 = percentile(&latencies, 50);
    let p95 = percentile(&latencies, 95);
    let p99 = percentile(&latencies, 99);

    println!("fetch_recent_verdicts(N={N_TASKS}): p50={p50:?} p95={p95:?} p99={p99:?}");

    assert!(
        p99 < P99_THRESHOLD,
        "p99 latency {p99:?} exceeded {P99_THRESHOLD:?} threshold across \
         {N_TASKS} parallel calls — issue #169 contention regression \
         (p50={p50:?}, p95={p95:?})"
    );
}
