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
use orchestrator_types::{Brief, Verdict};
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

    /// Fetch the typed `Verdict` for a specific brief by scanning the
    /// recent verdicts stream (XREVRANGE up to `scan_count`). There is no
    /// per-brief verdict key, so a small bounded scan is the cheapest
    /// path. Returns `None` when the brief is outside the scanned window
    /// or hasn't reached a terminal verdict. Used by the brief-239
    /// refusal-on-shipped fence in `metrics`.
    pub async fn fetch_verdict_for(
        &self,
        brief_id: &str,
        scan_count: usize,
    ) -> anyhow::Result<Option<Verdict>> {
        let mut conn = self.inner.conn.clone();
        let reply: redis::streams::StreamRangeReply = conn
            .xrevrange_count(VERDICTS_STREAM, "+", "-", scan_count)
            .await?;
        for entry in reply.ids {
            if let Some(body) = entry.map.get("verdict").and_then(redis_value_to_str) {
                if let Ok(v) = serde_json::from_str::<Verdict>(&body) {
                    if v.brief.0 == brief_id {
                        return Ok(Some(v));
                    }
                }
            }
        }
        Ok(None)
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
