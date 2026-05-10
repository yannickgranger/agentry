//! Integration tests for the per-brief abort CLI path (#473).
//!
//! Live-Redis like the rest of this crate's redis-integration suite:
//! gate on `AGENTRY_TEST_REDIS_URL` (default `redis://127.0.0.1:6380`)
//! and stay `#[ignore]` so the workspace-wide `cargo test` pass stays
//! green without a Redis dependency.
//!
//! Run with:
//! `cargo test --package orchestrator-runtime --test cli_abort -- --ignored`

use orchestrator_infra::config::RedisConfig;
use orchestrator_infra::{Config, Error};
use orchestrator_runtime::cli_abort::{run_per_brief_abort, ABORT_AGENT_ID};
use orchestrator_types::lifecycle::{
    BriefEvent, BriefState, BriefStateRecord, Reason, RetryBudget,
};
use orchestrator_types::{now, BriefId, Event, EventKind};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn cfg_with_url(url: &str) -> Config {
    Config {
        redis: RedisConfig {
            url: url.to_string(),
        },
        ..Config::default()
    }
}

async fn open_conn(url: &str) -> ConnectionManager {
    let client = redis::Client::open(url).expect("client");
    ConnectionManager::new(client).await.expect("conn")
}

async fn cleanup(conn: &mut ConnectionManager, brief_ids: &[&str]) {
    for id in brief_ids {
        let _: redis::RedisResult<()> = conn.del(format!("agentry:brief:{id}:state")).await;
        let _: redis::RedisResult<()> = conn.del(format!("agentry:brief:{id}:state_log")).await;
        let _: redis::RedisResult<()> = conn.del(format!("agentry:brief:{id}:trace")).await;
    }
}

fn fresh_retry() -> RetryBudget {
    RetryBudget { attempt: 1, max: 3 }
}

async fn seed_state(conn: &mut ConnectionManager, brief_id: &str, state: BriefState) {
    let record = BriefStateRecord {
        brief_id: BriefId(brief_id.into()),
        state,
        parent_brief_id: None,
        composition_role: None,
        at: now(),
    };
    let json = serde_json::to_string(&record).expect("serialize seed");
    let key = format!("agentry:brief:{brief_id}:state");
    let _: () = conn.set(&key, json.as_str()).await.expect("seed SET");
}

async fn read_state_raw(conn: &mut ConnectionManager, brief_id: &str) -> Option<String> {
    let key = format!("agentry:brief:{brief_id}:state");
    conn.get(&key).await.expect("read state")
}

async fn xlen(conn: &mut ConnectionManager, key: &str) -> i64 {
    let len: redis::RedisResult<i64> = redis::cmd("XLEN").arg(key).query_async(conn).await;
    len.unwrap_or_default()
}

async fn read_trace_brief_events(
    conn: &mut ConnectionManager,
    brief_id: &str,
) -> Vec<(String, BriefEvent)> {
    use redis::streams::StreamRangeReply;
    let key = format!("agentry:brief:{brief_id}:trace");
    let reply: redis::RedisResult<StreamRangeReply> = redis::cmd("XRANGE")
        .arg(&key)
        .arg("-")
        .arg("+")
        .query_async(conn)
        .await;
    let reply = match reply {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in reply.ids {
        let agent = match entry.map.get("agent") {
            Some(redis::Value::BulkString(b)) => {
                std::str::from_utf8(b).expect("utf8 agent").to_string()
            }
            Some(redis::Value::SimpleString(s)) => s.clone(),
            _ => continue,
        };
        let body = match entry.map.get("event") {
            Some(redis::Value::BulkString(b)) => {
                std::str::from_utf8(b).expect("utf8 event").to_string()
            }
            Some(redis::Value::SimpleString(s)) => s.clone(),
            _ => continue,
        };
        let event: Event = serde_json::from_str(&body).expect("parse Event");
        if let EventKind::Event { payload } = event.kind {
            if let Ok(brief_event) = serde_json::from_value::<BriefEvent>(payload) {
                out.push((agent, brief_event));
            }
        }
    }
    out
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn abort_pushes_event_for_active_brief() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_abort_active_001";
    cleanup(&mut conn, &[id]).await;

    let result = async {
        seed_state(
            &mut conn,
            id,
            BriefState::Authoring {
                agent_id: "agt_does_not_exist_001".into(),
                started_at: now(),
                retry: fresh_retry(),
            },
        )
        .await;

        let cfg = cfg_with_url(&url);
        run_per_brief_abort(&cfg, id, true, true)
            .await
            .expect("abort must succeed for active brief");

        let entries = read_trace_brief_events(&mut conn, id).await;
        assert!(
            entries.iter().any(|(agent, ev)| {
                agent == ABORT_AGENT_ID
                    && matches!(
                        ev,
                        BriefEvent::AbortRequested { actor, .. } if actor == "operator"
                    )
            }),
            "trace stream must contain at least one AbortRequested event with actor=operator pushed by {ABORT_AGENT_ID}, got {entries:?}",
        );

        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[id]).await;
    result.expect("test body");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn abort_terminal_brief_is_no_op() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_abort_terminal_001";
    cleanup(&mut conn, &[id]).await;

    let result = async {
        seed_state(
            &mut conn,
            id,
            BriefState::Failed {
                reason: Reason::BudgetExhausted,
            },
        )
        .await;

        let trace_key = format!("agentry:brief:{id}:trace");
        let before_xlen = xlen(&mut conn, &trace_key).await;
        let before_state = read_state_raw(&mut conn, id).await;

        let cfg = cfg_with_url(&url);
        run_per_brief_abort(&cfg, id, false, false)
            .await
            .expect("idempotent terminal abort must return Ok");

        let after_xlen = xlen(&mut conn, &trace_key).await;
        let after_state = read_state_raw(&mut conn, id).await;
        assert_eq!(
            before_xlen, after_xlen,
            "no new trace entry must be pushed for a terminal brief",
        );
        assert_eq!(
            before_state, after_state,
            "Failed seed must not be rewritten by an idempotent abort",
        );

        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[id]).await;
    result.expect("test body");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn abort_unknown_brief_errors() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_abort_unknown_001";
    cleanup(&mut conn, &[id]).await;

    let cfg = cfg_with_url(&url);
    let outcome = run_per_brief_abort(&cfg, id, true, true).await;
    cleanup(&mut conn, &[id]).await;

    match outcome {
        Err(Error::NotFound { kind: "brief", key }) => {
            assert_eq!(key, id, "NotFound must carry the requested brief id");
        }
        Err(other) => panic!("expected NotFound error for missing :state, got {other:?}"),
        Ok(()) => panic!("expected Err for missing :state, got Ok"),
    }
}
