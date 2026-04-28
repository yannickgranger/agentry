//! Integration test for the dispatcher's per-project concurrency cap
//! (EPIC #157 Seam 1).
//!
//! Models the daemon's dispatch shape — a per-project `HashMap<String,
//! Arc<Semaphore>>`, acquire-permit-then-spawn — without standing up a real
//! Redis or a real PodmanSpawner. A locally-defined mock spawner increments
//! a shared `AtomicUsize` on entry and decrements on exit, recording the
//! peak concurrent count via a CAS loop. The assertions pin that the cap is
//! actually enforced: with `Some(2)` the peak never exceeds 2; with the
//! `_global` pool at cap 3 the peak never exceeds 3.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

#[derive(Clone)]
struct MockSpawner {
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl MockSpawner {
    fn new() -> Self {
        Self {
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn run_agent(&self) {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let mut prev = self.peak.load(Ordering::SeqCst);
        while now > prev {
            match self
                .peak
                .compare_exchange(prev, now, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(p) => prev = p,
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Drive `count` briefs through the daemon's per-project acquire-then-spawn
/// pattern. `slug` is `"_global"` for projectless briefs.
async fn dispatch_n(spawner: &MockSpawner, slug: &str, cap: u32, count: usize) {
    let mut project_semaphores: HashMap<String, Arc<Semaphore>> = HashMap::new();
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let sem = project_semaphores
            .entry(slug.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(cap as usize)))
            .clone();
        let permit = sem
            .acquire_owned()
            .await
            .expect("semaphore not closed in test");
        let spawner = spawner.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            spawner.run_agent().await;
        }));
    }
    for h in handles {
        h.await.expect("spawned task joined");
    }
}

#[tokio::test]
async fn project_cap_holds_peak_in_flight_at_two() {
    let spawner = MockSpawner::new();
    dispatch_n(&spawner, "p_capped", 2, 5).await;
    let peak = spawner.peak.load(Ordering::SeqCst);
    assert!(
        peak <= 2,
        "per-project cap of 2 must hold; observed peak={peak}"
    );
    assert!(
        peak >= 1,
        "expected the briefs to actually run; peak={peak}"
    );
}

#[tokio::test]
async fn global_pool_holds_peak_in_flight_at_three() {
    let spawner = MockSpawner::new();
    dispatch_n(&spawner, "_global", 3, 5).await;
    let peak = spawner.peak.load(Ordering::SeqCst);
    assert!(peak <= 3, "global cap of 3 must hold; observed peak={peak}");
    assert!(
        peak >= 1,
        "expected the briefs to actually run; peak={peak}"
    );
}
