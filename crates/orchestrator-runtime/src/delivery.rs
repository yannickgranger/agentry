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
