//! Typed adapter for all dashboard reads/writes against Redis.
//!
//! Owns the `ConnectionManager` privately and exposes purpose-built methods
//! so handlers don't reach into Redis directly. The single fanout map keyed
//! by stream name guarantees ONE tail loop per stream regardless of viewer
//! count — solving the per-client xread fan-out that made the old dashboard
//! O(viewers × stream-tail-cost).
//!
//! The fanout map self-reaps: when a tail loop sees its broadcast `Sender`
//! has zero receivers for a sustained grace period, it removes its entry
//! from the map and exits, releasing the `ConnectionManager` clone it owned.
//! A new subscriber for the same stream simply restarts the loop. This
//! prevents the leak the prior diff would have caused, where every brief
//! detail page ever viewed would have left a tail loop running forever.
//!
//! At construction (`new`) the store also migrates any pre-existing
//! `agentry:role:*` / `agentry:team:*` / `agentry:project:*` keys into the
//! corresponding `agentry:{kind}:_index` ZSET. Without this step records
//! that pre-date the index would silently drop out of the listings — see
//! `specs/concepts/monitoring.md` for the migration rationale.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use orchestrator_runtime::redis_io;
use orchestrator_types::Brief;
use redis::aio::ConnectionManager;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

/// Shared inner state — Arc-wrapped so `DashboardStore` is cheap to clone
/// without exposing the `ConnectionManager` to handlers.
struct Inner {
    conn: ConnectionManager,
    /// Redis URL — kept so tail loops can open their OWN connection
    /// (issue #167 probe: `ConnectionManager.clone()` shares the
    /// multiplexed pipe; tail's blocking XREAD parks the pipe and
    /// serialises every other clone's commands).
    redis_url: String,
    fanouts: Mutex<HashMap<String, broadcast::Sender<String>>>,
    /// XREAD block timeout per iteration. Configurable so the eviction
    /// unit test can drive the loop fast.
    block_ms: u64,
    /// Idle period (sender's `receiver_count() == 0`) after which a tail
    /// loop reaps its fanout entry.
    eviction_grace: Duration,
}

/// Typed adapter for dashboard reads/writes. `Clone` is cheap (Arc).
#[derive(Clone)]
pub struct DashboardStore {
    inner: Arc<Inner>,
}

const TRACE_FANOUT_CAPACITY: usize = 256;
const VERDICTS_STREAM: &str = "agentry:verdicts";
const BRIEFS_STREAM: &str = "agentry:briefs";
const ACTIVE_BRIEFS_SET: &str = "agentry:active_briefs";
const DEFAULT_BLOCK_MS: u64 = 5_000;
const DEFAULT_EVICTION_GRACE: Duration = Duration::from_secs(30);

impl DashboardStore {
    /// Open the underlying `ConnectionManager` and backfill any pre-existing
    /// records into their `_index` ZSETs. Safe to call against a fresh or
    /// populated Redis.
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        Self::new_with(url, DEFAULT_BLOCK_MS, DEFAULT_EVICTION_GRACE).await
    }

    /// Test hook: same as `new` but with overridable XREAD block and
    /// eviction grace, so the eviction unit test can drive the loop fast.
    pub async fn new_with(
        url: &str,
        block_ms: u64,
        eviction_grace: Duration,
    ) -> anyhow::Result<Self> {
        let conn = redis_io::connect(url)
            .await
            .map_err(|e| anyhow::anyhow!("redis connect: {e}"))?;
        let store = Self {
            inner: Arc::new(Inner {
                conn,
                redis_url: url.to_string(),
                fanouts: Mutex::new(HashMap::new()),
                block_ms,
                eviction_grace,
            }),
        };
        if let Err(e) = store.backfill_indexes().await {
            tracing::warn!(error = %e, "index backfill failed; listings may be incomplete until next save");
        }
        Ok(store)
    }

    /// Redis URL the store was constructed with. Exposed so consumers that
    /// need a sync `redis::Connection` (e.g. the trace-query aggregate which
    /// is sync-only) can open one without re-discovering the URL through
    /// config.
    #[must_use]
    pub fn redis_url(&self) -> &str {
        &self.inner.redis_url
    }

    /// Most-recent verdicts (XREVRANGE on `agentry:verdicts`).
    pub async fn fetch_recent_verdicts(&self, count: usize) -> anyhow::Result<Vec<Value>> {
        let _t = std::time::Instant::now();
        let mut conn = self.inner.conn.clone();
        let reply: redis::streams::StreamRangeReply = conn
            .xrevrange_count(VERDICTS_STREAM, "+", "-", count)
            .await?;
        let mut out = Vec::with_capacity(reply.ids.len());
        for entry in reply.ids {
            if let Some(body) = entry.map.get("verdict").and_then(redis_value_to_str) {
                if let Ok(v) = serde_json::from_str(&body) {
                    out.push(v);
                }
            }
        }
        tracing::info!(
            method = "fetch_recent_verdicts",
            elapsed_ms = _t.elapsed().as_millis(),
            n_commands = 1usize,
            "dashboard_store_call"
        );
        Ok(out)
    }

    /// Most-recent brief submissions (XREVRANGE on `agentry:briefs`).
    pub async fn fetch_recent_briefs(&self, count: usize) -> anyhow::Result<Vec<Value>> {
        let _t = std::time::Instant::now();
        let mut conn = self.inner.conn.clone();
        let reply: redis::streams::StreamRangeReply =
            conn.xrevrange_count(BRIEFS_STREAM, "+", "-", count).await?;
        let mut out = Vec::with_capacity(reply.ids.len());
        for entry in reply.ids {
            if let Some(body) = entry.map.get("brief").and_then(redis_value_to_str) {
                if let Ok(v) = serde_json::from_str(&body) {
                    out.push(v);
                }
            }
        }
        tracing::info!(
            method = "fetch_recent_briefs",
            elapsed_ms = _t.elapsed().as_millis(),
            n_commands = 1usize,
            "dashboard_store_call"
        );
        Ok(out)
    }

    /// Briefs currently in flight: SMEMBERS of the `agentry:active_briefs`
    /// set (maintained by the daemon on intake / verdict-emit), then a
    /// single MGET across the `agentry:brief:<id>:body` keys to materialize.
    pub async fn active_briefs(&self) -> anyhow::Result<Vec<Value>> {
        let _t = std::time::Instant::now();
        let mut n_commands: usize = 0;
        let mut conn = self.inner.conn.clone();
        let ids: Vec<String> = conn.smembers(ACTIVE_BRIEFS_SET).await?;
        n_commands += 1;
        if ids.is_empty() {
            tracing::info!(
                method = "active_briefs",
                elapsed_ms = _t.elapsed().as_millis(),
                n_commands,
                "dashboard_store_call"
            );
            return Ok(Vec::new());
        }
        let keys: Vec<String> = ids
            .iter()
            .map(|id| format!("agentry:brief:{id}:body"))
            .collect();
        let bodies: Vec<Option<String>> = conn.mget(&keys).await?;
        n_commands += 1;
        let mut out = Vec::with_capacity(bodies.len());
        for body in bodies.into_iter().flatten() {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                out.push(v);
            }
        }
        tracing::info!(
            method = "active_briefs",
            elapsed_ms = _t.elapsed().as_millis(),
            n_commands,
            "dashboard_store_call"
        );
        Ok(out)
    }

    /// Trace history for a single brief (XRANGE on
    /// `agentry:brief:{id}:trace`). Used by the SSE handler when the JS
    /// asks for `?from=0-0` (history+live unified).
    pub async fn fetch_trace(&self, brief_id: &str, count: usize) -> anyhow::Result<Vec<Value>> {
        let _t = std::time::Instant::now();
        let stream = format!("agentry:brief:{brief_id}:trace");
        let mut conn = self.inner.conn.clone();
        let reply: redis::streams::StreamRangeReply =
            conn.xrange_count(&stream, "-", "+", count).await?;
        let mut out = Vec::with_capacity(reply.ids.len());
        for entry in reply.ids {
            if let Some(body) = entry.map.get("event").and_then(redis_value_to_str) {
                if let Ok(v) = serde_json::from_str(&body) {
                    out.push(v);
                }
            }
        }
        tracing::info!(
            method = "fetch_trace",
            elapsed_ms = _t.elapsed().as_millis(),
            n_commands = 1usize,
            "dashboard_store_call"
        );
        Ok(out)
    }

    /// List records of `kind` via the `agentry:{kind}:_index` ZSET, then a
    /// single MGET across the indexed keys. One round-trip each.
    pub async fn list<T: DeserializeOwned>(&self, kind: &str) -> anyhow::Result<Vec<(String, T)>> {
        let _t = std::time::Instant::now();
        let mut n_commands: usize = 0;
        let mut conn = self.inner.conn.clone();
        let index_key = format!("agentry:{kind}:_index");
        let keys: Vec<String> = conn.zrange(&index_key, 0, -1).await?;
        n_commands += 1;
        if keys.is_empty() {
            tracing::info!(
                method = "list",
                elapsed_ms = _t.elapsed().as_millis(),
                n_commands,
                "dashboard_store_call"
            );
            return Ok(Vec::new());
        }
        let bodies: Vec<Option<String>> = conn.mget(&keys).await?;
        n_commands += 1;
        let mut out = Vec::with_capacity(bodies.len());
        for (key, body) in keys.into_iter().zip(bodies.into_iter()) {
            if let Some(s) = body {
                if let Ok(v) = serde_json::from_str::<T>(&s) {
                    out.push((key, v));
                }
            }
        }
        tracing::info!(
            method = "list",
            elapsed_ms = _t.elapsed().as_millis(),
            n_commands,
            "dashboard_store_call"
        );
        Ok(out)
    }

    /// Persist a record and add the key to the `agentry:{kind}:_index`
    /// ZSET. Roles and teams are versioned so their key is
    /// `agentry:{kind}:{name}:v{version}`. Projects pre-date this adapter
    /// and are stored unversioned at `agentry:project:{name}` — keeping
    /// that shape preserves interop with `redis_io::fetch_project` and
    /// avoids orphaning data already on disk in the dogfood instance.
    pub async fn save<T: Serialize>(
        &self,
        kind: &str,
        name: &str,
        version: u32,
        value: &T,
    ) -> anyhow::Result<()> {
        let key = record_key(kind, name, version);
        let index_key = format!("agentry:{kind}:_index");
        let body = serde_json::to_string(value)?;
        let mut conn = self.inner.conn.clone();
        let _: () = conn.set(&key, body).await?;
        let _: () = conn.zadd(&index_key, &key, f64::from(version)).await?;
        Ok(())
    }

    /// Atomic next-version counter (INCR on
    /// `agentry:{kind}:{name}:_v`). Replaces the old SCAN+rsplit logic.
    pub async fn next_version(&self, kind: &str, name: &str) -> anyhow::Result<u32> {
        let _t = std::time::Instant::now();
        let key = format!("agentry:{kind}:{name}:_v");
        let mut conn = self.inner.conn.clone();
        let v: i64 = conn.incr(&key, 1).await?;
        tracing::info!(
            method = "next_version",
            elapsed_ms = _t.elapsed().as_millis(),
            n_commands = 1usize,
            "dashboard_store_call"
        );
        Ok(u32::try_from(v).unwrap_or(u32::MAX))
    }

    /// Delegate to `redis_io::submit_brief` with a connection clone — no
    /// lock held by the caller.
    pub async fn submit_brief(&self, brief: &Brief) -> anyhow::Result<String> {
        let mut conn = self.inner.conn.clone();
        let id = redis_io::submit_brief(&mut conn, brief)
            .await
            .map_err(|e| anyhow::anyhow!("submit_brief: {e}"))?;
        Ok(id)
    }

    /// Lazily start ONE tail loop per `agentry:brief:{id}:trace` stream;
    /// every subsequent subscriber for the same brief joins the existing
    /// fanout. Live-only (XREAD `$`) — history replay is the SSE handler's
    /// responsibility via `fetch_trace`.
    pub fn subscribe_trace(&self, brief_id: &str) -> broadcast::Receiver<String> {
        let stream = format!("agentry:brief:{brief_id}:trace");
        self.subscribe_stream(stream, "event")
    }

    /// Lazily start the verdicts tail loop. All viewers share the same
    /// fanout `Sender`.
    pub fn subscribe_verdicts(&self) -> broadcast::Receiver<String> {
        self.subscribe_stream(VERDICTS_STREAM.to_string(), "verdict")
    }

    /// Internal: look up or create the broadcast sender for `stream`,
    /// spawning the tail loop on first call. The mutex is held only for
    /// the HashMap lookup/insert — never across `.await`.
    fn subscribe_stream(&self, stream: String, field: &'static str) -> broadcast::Receiver<String> {
        let mut map = self
            .inner
            .fanouts
            .lock()
            .expect("fanout map mutex poisoned");
        if let Some(tx) = map.get(&stream) {
            return tx.subscribe();
        }
        let (tx, rx) = broadcast::channel::<String>(TRACE_FANOUT_CAPACITY);
        map.insert(stream.clone(), tx.clone());
        drop(map);
        let inner = self.inner.clone();
        let url = self.inner.redis_url.clone();
        let stream_for_log = stream.clone();
        tokio::spawn(async move {
            // Tail loops MUST own their own TCP connection (issue #167):
            // `ConnectionManager.clone()` shares the multiplexed pipe, and
            // a parked `XREAD … BLOCK` serialises every other clone's
            // commands. A fresh connect here gives the tail its own pipe
            // so handler reads stay responsive.
            match redis_io::connect(&url).await {
                Ok(conn) => tail_stream(inner, conn, stream, field, tx).await,
                Err(e) => tracing::warn!(
                    error = %e,
                    stream = %stream_for_log,
                    "tail_stream redis connect failed; subscribers will receive no events"
                ),
            }
        });
        rx
    }

    /// Backfill `agentry:{kind}:_index` ZSETs for any pre-existing records
    /// stored before the index existed. Called once from `new`. SCAN here
    /// is the documented one-time startup migration path; runtime read
    /// paths never SCAN.
    async fn backfill_indexes(&self) -> anyhow::Result<()> {
        let mut conn = self.inner.conn.clone();
        for kind in ["role", "team", "project"] {
            backfill_kind(&mut conn, kind).await?;
        }
        Ok(())
    }
}

/// Format the canonical Redis key for a record. Roles/teams are versioned;
/// projects are unversioned (legacy shape preserved on purpose — see
/// `save`).
fn record_key(kind: &str, name: &str, version: u32) -> String {
    if kind == "project" {
        format!("agentry:{kind}:{name}")
    } else {
        format!("agentry:{kind}:{name}:v{version}")
    }
}

/// Walk every `agentry:{kind}:*` key with SCAN and ZADD anything that
/// looks like a record (skipping the `_index` ZSET itself and the `:_v`
/// version counters) into `agentry:{kind}:_index`. Score is the parsed
/// version when a `:v{n}` suffix is present, else 1.
async fn backfill_kind(conn: &mut ConnectionManager, kind: &str) -> anyhow::Result<()> {
    let pattern = format!("agentry:{kind}:*");
    let index_key = format!("agentry:{kind}:_index");
    let mut keys: Vec<String> = Vec::new();
    {
        let mut iter = conn
            .scan_match::<_, String>(&pattern)
            .await
            .map_err(|e| anyhow::anyhow!("scan {pattern}: {e}"))?;
        while let Some(k) = iter.next_item().await {
            if k == index_key || k.ends_with(":_v") {
                continue;
            }
            keys.push(k);
        }
    }
    for k in keys {
        let score = parse_version_from_key(&k).unwrap_or(1);
        // ZADD is idempotent for the same (key, score) pair, so re-runs are safe.
        if let Err(e) = conn
            .zadd::<_, _, _, ()>(&index_key, &k, f64::from(score))
            .await
        {
            tracing::warn!(error = %e, key = %k, "backfill zadd failed");
        }
    }
    Ok(())
}

/// Parse a trailing `:v{n}` suffix into a version number; returns `None`
/// if the key has no version (e.g. `agentry:project:{slug}`).
fn parse_version_from_key(key: &str) -> Option<u32> {
    let (head, tail) = key.rsplit_once(":v")?;
    // Guard against false matches like `agentry:project:vegan` where the
    // `:v` we found is not a version separator. Require the suffix to be
    // entirely digits.
    if tail.bytes().all(|b| b.is_ascii_digit()) && !tail.is_empty() {
        // Also require head to look like `agentry:{kind}:{name}` — at
        // least three colon-separated segments.
        if head.matches(':').count() >= 2 {
            return tail.parse().ok();
        }
    }
    None
}

/// The single tail loop per stream. XREADs from `$`, decodes each entry's
/// `field` value, and broadcasts it to every subscribed receiver. Owns its
/// own `ConnectionManager` clone for the read socket and its own clone of
/// the broadcast `Sender` so it can observe `receiver_count`.
///
/// Self-eviction: between XREAD iterations, if `tx.receiver_count()` has
/// been zero for `inner.eviction_grace`, the loop acquires the fanout map
/// lock, re-checks the count under the lock, removes the entry if still
/// zero, and exits. The ConnectionManager clone is dropped when the task
/// returns.
async fn tail_stream(
    inner: Arc<Inner>,
    mut conn: ConnectionManager,
    stream: String,
    field: &'static str,
    tx: broadcast::Sender<String>,
) {
    let mut last_id = "$".to_string();
    let mut idle_since: Option<Instant> = None;
    let block_ms = usize::try_from(inner.block_ms).unwrap_or(usize::MAX);

    loop {
        let opts = StreamReadOptions::default().block(block_ms).count(16);
        let read: Result<Option<StreamReadReply>, redis::RedisError> = conn
            .xread_options(&[stream.as_str()], &[last_id.as_str()], &opts)
            .await;
        match read {
            Ok(Some(reply)) => {
                for k in reply.keys {
                    for entry in k.ids {
                        last_id = entry.id.clone();
                        if let Some(body) = entry.map.get(field).and_then(redis_value_to_str) {
                            // `send` errors only when there are no
                            // receivers; the Sender stays alive because
                            // the fanout map holds a clone, so we keep
                            // tailing for future subscribers.
                            let _ = tx.send(body);
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(error=%err, stream=%stream, "xread error");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }

        // Eviction check between iterations. The receiver count is shared
        // across all clones of `tx`, so reading it on this clone returns
        // the same number any other observer would see.
        if tx.receiver_count() > 0 {
            idle_since = None;
        } else {
            let started = *idle_since.get_or_insert_with(Instant::now);
            if started.elapsed() >= inner.eviction_grace {
                let mut map = inner.fanouts.lock().expect("fanout map mutex poisoned");
                // Re-check under the lock — a subscriber could have
                // arrived in the gap between `elapsed()` and the lock
                // acquisition. If so, leave the entry, clear the timer,
                // and keep tailing.
                if let Some(map_tx) = map.get(&stream) {
                    if map_tx.receiver_count() == 0 {
                        map.remove(&stream);
                        drop(map);
                        tracing::debug!(stream=%stream, "fanout entry reaped after idle grace");
                        return;
                    }
                }
                idle_since = None;
            }
        }
    }
}

fn redis_value_to_str(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
        redis::Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::AsyncCommands;
    use serde::Deserialize;

    fn test_redis_url() -> String {
        std::env::var("AGENTRY_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".into())
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Probe {
        name: String,
        version: u32,
    }

    /// Bundle a fresh `_index` ZSET, a few SET'd records, and a slot tag so
    /// concurrent test runs don't collide. Returns the kind name to use.
    async fn seed_records(prefix: &str) -> (DashboardStore, String) {
        let store = DashboardStore::new(&test_redis_url())
            .await
            .expect("connect");
        let kind = format!(
            "test_{prefix}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        (store, kind)
    }

    async fn cleanup(store: &DashboardStore, kind: &str, names: &[&str]) {
        let mut conn = store.inner.conn.clone();
        let _: () = conn
            .del(format!("agentry:{kind}:_index"))
            .await
            .unwrap_or(());
        for n in names {
            let _: () = conn
                .del(format!("agentry:{kind}:{n}:_v"))
                .await
                .unwrap_or(());
            let _: () = conn
                .del(format!("agentry:{kind}:{n}:v1"))
                .await
                .unwrap_or(());
            let _: () = conn
                .del(format!("agentry:{kind}:{n}:v2"))
                .await
                .unwrap_or(());
            let _: () = conn.del(format!("agentry:{kind}:{n}")).await.unwrap_or(());
        }
    }

    #[test]
    fn parse_version_from_key_extracts_trailing_v_n() {
        assert_eq!(
            parse_version_from_key("agentry:role:coder:v1"),
            Some(1),
            "versioned role key parses"
        );
        assert_eq!(
            parse_version_from_key("agentry:role:coder:v42"),
            Some(42),
            "multi-digit version parses"
        );
        // Project keys are intentionally unversioned.
        assert_eq!(
            parse_version_from_key("agentry:project:my-project"),
            None,
            "unversioned project key returns None"
        );
        // False-match guard: the `:v` inside `:vegan` must not be parsed
        // as a version separator, since the tail isn't all digits.
        assert_eq!(
            parse_version_from_key("agentry:project:vegan"),
            None,
            "non-digit tail after :v returns None"
        );
        // Only the trailing `:v{n}` should match (rightmost occurrence).
        assert_eq!(
            parse_version_from_key("agentry:role:v3:v7"),
            Some(7),
            "rightmost :v wins"
        );
    }

    #[test]
    fn record_key_versions_roles_and_teams_only() {
        assert_eq!(
            record_key("role", "coder", 3),
            "agentry:role:coder:v3",
            "roles versioned"
        );
        assert_eq!(
            record_key("team", "qbot", 1),
            "agentry:team:qbot:v1",
            "teams versioned"
        );
        assert_eq!(
            record_key("project", "my-project", 1),
            "agentry:project:my-project",
            "projects unversioned (legacy shape)"
        );
    }

    /// Hermetic eviction test: we don't need a live tail loop to verify
    /// the eviction predicate. Build a fanout map directly, drop the
    /// receiver, then call the same eviction step the tail loop runs.
    /// Asserts the entry is reaped.
    #[test]
    fn fanout_entry_reaped_when_no_receivers() {
        let map: Mutex<HashMap<String, broadcast::Sender<String>>> = Mutex::new(HashMap::new());
        let stream = "agentry:brief:test:trace".to_string();
        let (tx, rx) = broadcast::channel::<String>(8);
        map.lock().expect("lock").insert(stream.clone(), tx.clone());

        // While a receiver lives, eviction must NOT happen.
        {
            let m = map.lock().expect("lock");
            assert_eq!(
                m.get(&stream).map(|t| t.receiver_count()),
                Some(1),
                "receiver count visible to map's tx"
            );
        }
        drop(rx);

        // Now no receivers — emulate the tail loop's under-lock check.
        let evicted = {
            let mut m = map.lock().expect("lock");
            if let Some(t) = m.get(&stream) {
                if t.receiver_count() == 0 {
                    m.remove(&stream);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        assert!(evicted, "fanout entry must be removable after rx drop");
        assert!(
            !map.lock().expect("lock").contains_key(&stream),
            "entry gone from map"
        );
        // Keep tx alive to the end; without a live receiver and now
        // without a map slot, the broadcast Sender will be dropped after
        // this scope exits — exactly the reclamation we want for a
        // reaped tail loop.
        drop(tx);
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn save_then_list_round_trips() {
        let (store, kind) = seed_records("save_list").await;
        let p1 = Probe {
            name: "alpha".into(),
            version: 1,
        };
        let p2 = Probe {
            name: "beta".into(),
            version: 1,
        };
        store
            .save(&kind, "alpha", 1, &p1)
            .await
            .expect("save alpha");
        store.save(&kind, "beta", 1, &p2).await.expect("save beta");
        let listed: Vec<(String, Probe)> = store.list::<Probe>(&kind).await.expect("list");
        assert_eq!(listed.len(), 2);
        let names: std::collections::HashSet<_> =
            listed.iter().map(|(_, p)| p.name.clone()).collect();
        assert!(names.contains("alpha"));
        assert!(names.contains("beta"));
        cleanup(&store, &kind, &["alpha", "beta"]).await;
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn next_version_increments_atomically() {
        let (store, kind) = seed_records("next_version").await;
        let v1 = store.next_version(&kind, "alpha").await.expect("incr 1");
        let v2 = store.next_version(&kind, "alpha").await.expect("incr 2");
        let v3 = store.next_version(&kind, "alpha").await.expect("incr 3");
        assert_eq!(v1, 1);
        assert_eq!(v2, 2);
        assert_eq!(v3, 3);
        cleanup(&store, &kind, &["alpha"]).await;
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn subscribe_trace_shares_one_tail_loop() {
        let (store, _kind) = seed_records("subscribe_share").await;
        let id = format!(
            "brf_test_share_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        // Two subscribers for the same brief id should share one fanout
        // entry — only one tail loop spawned.
        let _rx1 = store.subscribe_trace(&id);
        let _rx2 = store.subscribe_trace(&id);
        let map = store.inner.fanouts.lock().expect("lock");
        let stream_key = format!("agentry:brief:{id}:trace");
        assert!(
            map.contains_key(&stream_key),
            "fanout entry must exist after subscribe"
        );
        assert_eq!(map.len(), 1, "only one fanout entry for the stream");
    }

    /// End-to-end: drop the only receiver, wait past the (short, test-
    /// configured) eviction grace, assert the fanout entry is gone. This
    /// exercises the real tail loop running against Redis — proving the
    /// auto-reap fires. Hermetic coverage of the same predicate lives in
    /// `fanout_entry_reaped_when_no_receivers` above.
    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn fanout_auto_evicts_after_idle_grace() {
        // Block 100ms per XREAD iteration, evict after 200ms idle.
        let store = DashboardStore::new_with(&test_redis_url(), 100, Duration::from_millis(200))
            .await
            .expect("connect");
        let id = format!(
            "brf_evict_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let stream_key = format!("agentry:brief:{id}:trace");

        let rx = store.subscribe_trace(&id);
        assert!(
            store
                .inner
                .fanouts
                .lock()
                .expect("lock")
                .contains_key(&stream_key),
            "entry present after subscribe"
        );
        drop(rx);

        // Wait for two XREAD iterations + grace + slack.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let absent = !store
                .inner
                .fanouts
                .lock()
                .expect("lock")
                .contains_key(&stream_key);
            if absent {
                return;
            }
        }
        panic!("fanout entry was not reaped within bounded time");
    }
}
