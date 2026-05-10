//! Integration tests for `captain decide accept|reject|list` (#449b).
//!
//! Live-Redis like the rest of this crate's redis-integration suite: gate on
//! `AGENTRY_TEST_REDIS_URL` (default `redis://127.0.0.1:6380`) and stay
//! `#[ignore]` so the workspace-wide `cargo test` pass stays green without
//! a Redis dependency.
//!
//! Run with:
//! `cargo test --package orchestrator-runtime --test cli_decide -- --ignored`

use orchestrator_infra::config::RedisConfig;
use orchestrator_infra::{Config, Error};
use orchestrator_runtime::cli_decide::{run_accept, run_list, run_reject, DECIDE_AGENT_ID};
use orchestrator_types::lifecycle::{
    BriefEvent, BriefState, BriefStateRecord, DisagreementSummary, Reason, RetryBudget,
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

fn parked_state() -> BriefState {
    BriefState::AwaitingCaptainDecision {
        disagreements: vec![DisagreementSummary {
            verb: "UPDATE foo.rs:10".into(),
            applied_form: "UPDATE foo.rs:10 (widened)".into(),
            rationale: "literal verb context too narrow".into(),
        }],
        retry: fresh_retry(),
    }
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
async fn decide_accept_pushes_captain_accepted() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_decide_accept_001";
    cleanup(&mut conn, &[id]).await;

    let result =
        async {
            seed_state(&mut conn, id, parked_state()).await;
            let cfg = cfg_with_url(&url);
            run_accept(&cfg, id)
                .await
                .expect("decide accept must succeed for parked brief");

            let entries = read_trace_brief_events(&mut conn, id).await;
            assert!(
                entries.iter().any(|(agent, ev)| agent == DECIDE_AGENT_ID
                    && matches!(ev, BriefEvent::CaptainAccepted)),
                "trace stream must carry CaptainAccepted from {DECIDE_AGENT_ID}, got {entries:?}",
            );
            Ok::<(), String>(())
        }
        .await;

    cleanup(&mut conn, &[id]).await;
    result.expect("test body");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn decide_reject_pushes_captain_rejected_with_reason() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_decide_reject_001";
    cleanup(&mut conn, &[id]).await;

    let result = async {
        seed_state(&mut conn, id, parked_state()).await;
        let cfg = cfg_with_url(&url);
        run_reject(&cfg, id, "captain prefers literal verb")
            .await
            .expect("decide reject must succeed for parked brief");

        let entries = read_trace_brief_events(&mut conn, id).await;
        assert!(
            entries
                .iter()
                .any(|(agent, ev)| agent == DECIDE_AGENT_ID
                    && matches!(
                        ev,
                        BriefEvent::CaptainRejected { reason } if reason == "captain prefers literal verb"
                    )),
            "trace stream must carry CaptainRejected{{reason}} from {DECIDE_AGENT_ID}, got {entries:?}",
        );
        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[id]).await;
    result.expect("test body");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn decide_on_unparked_brief_errors() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_decide_unparked_001";
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
        let cfg = cfg_with_url(&url);
        let outcome = run_accept(&cfg, id).await;
        match outcome {
            Err(Error::Config(msg)) => {
                assert!(
                    msg.contains("not parked"),
                    "expected 'not parked' message, got: {msg}",
                );
            }
            Err(other) => panic!("expected Config error for unparked brief, got {other:?}"),
            Ok(()) => panic!("expected Err for unparked brief, got Ok"),
        }
        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[id]).await;
    result.expect("test body");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn decide_list_finds_parked_briefs() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let parked_a = "test_decide_list_parked_a";
    let parked_b = "test_decide_list_parked_b";
    let unparked = "test_decide_list_failed_c";
    cleanup(&mut conn, &[parked_a, parked_b, unparked]).await;

    let result = async {
        seed_state(&mut conn, parked_a, parked_state()).await;
        seed_state(&mut conn, parked_b, parked_state()).await;
        seed_state(
            &mut conn,
            unparked,
            BriefState::Failed {
                reason: Reason::BudgetExhausted,
            },
        )
        .await;

        let cfg = cfg_with_url(&url);
        // run_list prints to stdout — the integration test asserts on the
        // underlying parked-set instead, since stdout capture in tokio
        // tests is brittle. The function is exercised end-to-end (Redis
        // SCAN + GET + filter), and a panic in any of those steps will
        // propagate as a test failure.
        run_list(&cfg).await.expect("decide list must succeed");

        // Cross-check the SCAN output from the same connection.
        let mut cursor: u64 = 0;
        let mut found: Vec<String> = Vec::new();
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg("agentry:brief:test_decide_list_*:state")
                .arg("COUNT")
                .arg(100)
                .query_async(&mut conn)
                .await
                .expect("SCAN");
            for k in batch {
                let raw: Option<String> = conn.get(&k).await.expect("GET");
                let Some(raw) = raw else { continue };
                let record: BriefStateRecord =
                    serde_json::from_str(&raw).expect("parse BriefStateRecord");
                if matches!(record.state, BriefState::AwaitingCaptainDecision { .. }) {
                    found.push(record.brief_id.0);
                }
            }
            if next == 0 {
                break;
            }
            cursor = next;
        }
        found.sort();
        assert_eq!(
            found,
            vec![parked_a.to_string(), parked_b.to_string()],
            "list must surface both parked briefs and skip the Failed seed",
        );
        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[parked_a, parked_b, unparked]).await;
    result.expect("test body");
}
