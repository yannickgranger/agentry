//! Delivery — per-brief Redis projection of PR/CI/merge state.
//!
//! Single source of truth for delivery observability across DOL,
//! ci-watcher, and rework. The daemon mirrors trace events into a
//! `agentry:delivery:<brief_id>` hash plus an append-only
//! `agentry:delivery:<brief_id>:attempts` list.
//!
//! Best-effort: failures here must never fail a brief. Spawner calls
//! this fire-and-forget and discards the result.

use orchestrator_types::{BriefId, Event, EventKind};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::json;

pub async fn record(
    conn: &mut ConnectionManager,
    brief_id: &BriefId,
    _agent_id: &str,
    event: &Event,
) -> anyhow::Result<()> {
    let hash_key = format!("agentry:delivery:{}", brief_id.0);
    let attempts_key = format!("agentry:delivery:{}:attempts", brief_id.0);
    let at = event.at.to_rfc3339();

    match &event.kind {
        EventKind::Event { payload } => {
            let msg = payload.get("msg").and_then(|v| v.as_str()).unwrap_or("");
            match msg {
                "PR opened" => {
                    if let Some(n) = payload.get("number") {
                        let _: () = conn.hset(&hash_key, "pr_number", n.to_string()).await?;
                    }
                    if let Some(u) = payload.get("url").and_then(|v| v.as_str()) {
                        let _: () = conn.hset(&hash_key, "pr_url", u).await?;
                    }
                    if let Some(b) = payload.get("branch").and_then(|v| v.as_str()) {
                        let _: () = conn.hset(&hash_key, "branch", b).await?;
                    }
                    if let Some(s) = payload.get("head_sha").and_then(|v| v.as_str()) {
                        let _: () = conn.hset(&hash_key, "head_sha", s).await?;
                    }
                }
                "polling CI" => {
                    let state = payload.get("state").and_then(|v| v.as_str()).unwrap_or("");
                    let iteration = payload.get("iteration").cloned().unwrap_or(json!(0));
                    let _: () = conn.hset(&hash_key, "ci_state", state).await?;
                    let envelope = json!({
                        "kind": "ci_poll",
                        "state": state,
                        "iteration": iteration,
                        "at": at,
                    });
                    let _: () = conn.rpush(&attempts_key, envelope.to_string()).await?;
                }
                "merged" => {
                    let _: () = conn.hset(&hash_key, "merged", "true").await?;
                    let envelope = json!({
                        "kind": "merge_attempt",
                        "outcome": "success",
                        "at": at,
                    });
                    let _: () = conn.rpush(&attempts_key, envelope.to_string()).await?;
                }
                "merge transient failure — retrying" => {
                    let http_code = payload.get("http_code").cloned().unwrap_or(json!(""));
                    let attempt = payload.get("merge_attempt").cloned().unwrap_or(json!(0));
                    let envelope = json!({
                        "kind": "merge_attempt",
                        "outcome": "transient",
                        "http_code": http_code,
                        "attempt": attempt,
                        "at": at,
                    });
                    let _: () = conn.rpush(&attempts_key, envelope.to_string()).await?;
                }
                _ => {}
            }
        }
        EventKind::Finding { finding } => {
            let target = finding
                .file
                .clone()
                .unwrap_or_else(|| finding.category.clone());
            let envelope = json!({
                "kind": "rework",
                "target": target,
                "reason": finding.message,
                "at": at,
            });
            let _: () = conn.rpush(&attempts_key, envelope.to_string()).await?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_types::{Event, EventKind};
    use serde_json::json;

    async fn fresh_conn() -> ConnectionManager {
        let url = std::env::var("AGENTRY_TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6380".into());
        let client = redis::Client::open(url).expect("redis client");
        ConnectionManager::new(client).await.expect("redis connect")
    }

    fn fresh_brief_id(prefix: &str) -> BriefId {
        BriefId(format!(
            "brf_test_delivery_{}_{}",
            prefix,
            uuid::Uuid::now_v7()
        ))
    }

    async fn cleanup(conn: &mut ConnectionManager, brief_id: &BriefId) {
        let _: () = conn
            .del(format!("agentry:delivery:{}", brief_id.0))
            .await
            .unwrap_or(());
        let _: () = conn
            .del(format!("agentry:delivery:{}:attempts", brief_id.0))
            .await
            .unwrap_or(());
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn record_extracts_pr_fields_from_pr_opened() {
        let mut conn = fresh_conn().await;
        let brief_id = fresh_brief_id("pr_opened");
        let ev = Event::new(EventKind::Event {
            payload: json!({
                "msg": "PR opened",
                "number": 42,
                "url": "https://agency.lab:3000/yg/agentry/pulls/42",
                "branch": "auto/brf_test",
                "head_sha": "abc123",
            }),
        });

        record(&mut conn, &brief_id, "shipper-agentry", &ev)
            .await
            .expect("record");

        let key = format!("agentry:delivery:{}", brief_id.0);
        let pr_number: String = conn.hget(&key, "pr_number").await.expect("hget pr_number");
        let pr_url: String = conn.hget(&key, "pr_url").await.expect("hget pr_url");
        let branch: String = conn.hget(&key, "branch").await.expect("hget branch");
        let head_sha: String = conn.hget(&key, "head_sha").await.expect("hget head_sha");
        assert_eq!(pr_number, "42");
        assert_eq!(pr_url, "https://agency.lab:3000/yg/agentry/pulls/42");
        assert_eq!(branch, "auto/brf_test");
        assert_eq!(head_sha, "abc123");

        cleanup(&mut conn, &brief_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn record_appends_ci_poll_attempts() {
        let mut conn = fresh_conn().await;
        let brief_id = fresh_brief_id("ci_poll");

        for i in 1..=3 {
            let ev = Event::new(EventKind::Event {
                payload: json!({"msg": "polling CI", "state": "pending", "iteration": i}),
            });
            record(&mut conn, &brief_id, "ci-watcher-agentry", &ev)
                .await
                .expect("record");
        }

        let attempts_key = format!("agentry:delivery:{}:attempts", brief_id.0);
        let entries: Vec<String> = conn
            .lrange(&attempts_key, 0, -1)
            .await
            .expect("lrange attempts");
        assert_eq!(entries.len(), 3);
        for (i, entry) in entries.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(entry).expect("parse envelope");
            assert_eq!(v["kind"], "ci_poll");
            assert_eq!(v["state"], "pending");
            assert_eq!(v["iteration"], i as i64 + 1);
        }

        cleanup(&mut conn, &brief_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
    async fn record_records_merge_transient() {
        let mut conn = fresh_conn().await;
        let brief_id = fresh_brief_id("merge_transient");

        let ev = Event::new(EventKind::Event {
            payload: json!({
                "msg": "merge transient failure — retrying",
                "http_code": "409",
                "merge_attempt": 2,
                "sleep_seconds": 12,
            }),
        });
        record(&mut conn, &brief_id, "ci-watcher-agentry", &ev)
            .await
            .expect("record");

        let attempts_key = format!("agentry:delivery:{}:attempts", brief_id.0);
        let entries: Vec<String> = conn
            .lrange(&attempts_key, 0, -1)
            .await
            .expect("lrange attempts");
        assert_eq!(entries.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&entries[0]).expect("parse envelope");
        assert_eq!(v["kind"], "merge_attempt");
        assert_eq!(v["outcome"], "transient");
        assert_eq!(v["http_code"], "409");

        cleanup(&mut conn, &brief_id).await;
    }
}
