//! Typed adapter for all dashboard reads/writes against Redis.
//!
//! Owns the `ConnectionManager` privately and exposes purpose-built methods
//! so handlers don't reach into Redis directly. The single fanout map keyed
//! by stream name guarantees ONE tail loop per stream regardless of viewer
//! count — solving the per-client xread fan-out that made the old dashboard
//! O(viewers × stream-tail-cost).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    fanouts: Mutex<HashMap<String, broadcast::Sender<String>>>,
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

impl DashboardStore {
    /// Open the underlying `ConnectionManager`. The manager is internally
    /// `Arc`-multiplexed, so every clone we hand out shares one socket.
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        let conn = redis_io::connect(url)
            .await
            .map_err(|e| anyhow::anyhow!("redis connect: {e}"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                conn,
                fanouts: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Most-recent verdicts (XREVRANGE on `agentry:verdicts`).
    pub async fn fetch_recent_verdicts(&self, count: usize) -> anyhow::Result<Vec<Value>> {
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
        Ok(out)
    }

    /// Most-recent brief submissions (XREVRANGE on `agentry:briefs`).
    pub async fn fetch_recent_briefs(&self, count: usize) -> anyhow::Result<Vec<Value>> {
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
        Ok(out)
    }

    /// Briefs currently in flight: SMEMBERS of the `agentry:active_briefs`
    /// set (maintained by the daemon on intake / verdict-emit), then a
    /// single MGET across the `agentry:brief:<id>:body` keys to materialize.
    pub async fn active_briefs(&self) -> anyhow::Result<Vec<Value>> {
        let mut conn = self.inner.conn.clone();
        let ids: Vec<String> = conn.smembers(ACTIVE_BRIEFS_SET).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<String> = ids
            .iter()
            .map(|id| format!("agentry:brief:{id}:body"))
            .collect();
        let bodies: Vec<Option<String>> = conn.mget(&keys).await?;
        let mut out = Vec::with_capacity(bodies.len());
        for body in bodies.into_iter().flatten() {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Trace history for a single brief (XRANGE on
    /// `agentry:brief:{id}:trace`). Used by the SSE handler when the JS
    /// asks for `?from=0-0` (history+live unified).
    pub async fn fetch_trace(&self, brief_id: &str, count: usize) -> anyhow::Result<Vec<Value>> {
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
        Ok(out)
    }

    /// List records of `kind` via the `agentry:{kind}:_index` ZSET, then a
    /// single MGET across the indexed keys. One round-trip each.
    pub async fn list<T: DeserializeOwned>(&self, kind: &str) -> anyhow::Result<Vec<(String, T)>> {
        let mut conn = self.inner.conn.clone();
        let index_key = format!("agentry:{kind}:_index");
        let keys: Vec<String> = conn.zrange(&index_key, 0, -1).await?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let bodies: Vec<Option<String>> = conn.mget(&keys).await?;
        let mut out = Vec::with_capacity(bodies.len());
        for (key, body) in keys.into_iter().zip(bodies.into_iter()) {
            if let Some(s) = body {
                if let Ok(v) = serde_json::from_str::<T>(&s) {
                    out.push((key, v));
                }
            }
        }
        Ok(out)
    }

    /// Persist a record at `agentry:{kind}:{name}:v{version}` and add the
    /// key to the `agentry:{kind}:_index` ZSET so `list` can find it. The
    /// score uses the version so newest revisions sort last.
    pub async fn save<T: Serialize>(
        &self,
        kind: &str,
        name: &str,
        version: u32,
        value: &T,
    ) -> anyhow::Result<()> {
        let key = format!("agentry:{kind}:{name}:v{version}");
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
        let key = format!("agentry:{kind}:{name}:_v");
        let mut conn = self.inner.conn.clone();
        let v: i64 = conn.incr(&key, 1).await?;
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
        let conn = self.inner.conn.clone();
        tokio::spawn(tail_stream(conn, stream, field, tx));
        rx
    }
}

/// The single tail loop per stream. XREADs from `$`, decodes each entry's
/// `field` value, and broadcasts it to every subscribed receiver. Owns its
/// own `ConnectionManager` clone for the read socket; never touches the
/// fanout map.
async fn tail_stream(
    mut conn: ConnectionManager,
    stream: String,
    field: &'static str,
    tx: broadcast::Sender<String>,
) {
    let mut last_id = "$".to_string();
    loop {
        let opts = StreamReadOptions::default().block(5_000).count(16);
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
        }
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
}
