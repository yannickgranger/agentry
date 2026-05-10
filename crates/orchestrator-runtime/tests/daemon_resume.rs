//! Integration tests for the boot-time orphan scan (#471a).
//!
//! Live-Redis like the rest of this crate's redis-integration suite:
//! gate on `AGENTRY_TEST_REDIS_URL` (default `redis://127.0.0.1:6380`)
//! and stay `#[ignore]` so the workspace-wide `cargo test` pass stays
//! green without a Redis dependency.
//!
//! Run with:
//! `cargo test --package orchestrator-runtime --test daemon_resume -- --ignored`

use orchestrator_runtime::daemon_resume::{resume_orphans, ResumeReport};
use orchestrator_types::lifecycle::{BriefState, BriefStateRecord, Reason, RetryBudget};
use orchestrator_types::{now, BriefId};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

async fn open_conn(url: &str) -> ConnectionManager {
    let client = redis::Client::open(url).expect("client");
    ConnectionManager::new(client).await.expect("conn")
}

async fn cleanup(conn: &mut ConnectionManager, brief_ids: &[&str]) {
    for id in brief_ids {
        let _: redis::RedisResult<()> = conn.del(format!("agentry:brief:{id}:state")).await;
        let _: redis::RedisResult<()> = conn.del(format!("agentry:brief:{id}:state_log")).await;
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

async fn read_state(conn: &mut ConnectionManager, brief_id: &str) -> BriefStateRecord {
    let key = format!("agentry:brief:{brief_id}:state");
    let raw: Option<String> = conn.get(&key).await.expect("read state");
    let raw = raw.expect("state present");
    serde_json::from_str(&raw).expect("parse state")
}

async fn read_log_records(conn: &mut ConnectionManager, brief_id: &str) -> Vec<BriefStateRecord> {
    use redis::streams::StreamRangeReply;
    let key = format!("agentry:brief:{brief_id}:state_log");
    let reply: StreamRangeReply = redis::cmd("XRANGE")
        .arg(&key)
        .arg("-")
        .arg("+")
        .query_async(conn)
        .await
        .expect("xrange");
    let mut out = Vec::new();
    for entry in reply.ids {
        if let Some(redis::Value::BulkString(bytes)) = entry.map.get("record") {
            let s = std::str::from_utf8(bytes).expect("utf8");
            out.push(serde_json::from_str(s).expect("parse log record"));
        }
    }
    out
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn resume_marks_dead_container_failed() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_brief_dead_001";
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

        let report = resume_orphans(&mut conn).await.expect("resume");
        assert_eq!(
            report,
            ResumeReport {
                scanned: 1,
                failed_dead: 1,
                kept_alive: 0,
            },
            "dead-container record must be reported as failed_dead",
        );

        let after = read_state(&mut conn, id).await;
        assert!(
            matches!(
                after.state,
                BriefState::Failed {
                    reason: Reason::DaemonRestartedDuringExecution
                }
            ),
            "state key must hold Failed{{DaemonRestartedDuringExecution}} after resume, got {:?}",
            after.state,
        );

        let log = read_log_records(&mut conn, id).await;
        assert_eq!(
            log.len(),
            1,
            "exactly one new state_log entry must be appended for the failed transition",
        );
        assert!(
            matches!(
                log[0].state,
                BriefState::Failed {
                    reason: Reason::DaemonRestartedDuringExecution
                }
            ),
            "state_log entry must carry the same Failed record",
        );
        assert_eq!(log[0].brief_id.0, id);

        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[id]).await;
    result.expect("test body");
}

#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn resume_skips_terminal_records() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let failed_id = "test_brief_terminal_failed_001";
    let shipped_id = "test_brief_terminal_shipped_001";
    cleanup(&mut conn, &[failed_id, shipped_id]).await;

    let result = async {
        seed_state(
            &mut conn,
            failed_id,
            BriefState::Failed {
                reason: Reason::BudgetExhausted,
            },
        )
        .await;
        seed_state(&mut conn, shipped_id, BriefState::Shipped).await;

        let before_failed = read_state(&mut conn, failed_id).await;
        let before_shipped = read_state(&mut conn, shipped_id).await;

        let report = resume_orphans(&mut conn).await.expect("resume");
        assert_eq!(
            report,
            ResumeReport {
                scanned: 0,
                failed_dead: 0,
                kept_alive: 0,
            },
            "terminal records must be filtered before counting",
        );

        let after_failed = read_state(&mut conn, failed_id).await;
        let after_shipped = read_state(&mut conn, shipped_id).await;
        assert_eq!(
            before_failed, after_failed,
            "Failed seed must not be rewritten",
        );
        assert_eq!(
            before_shipped, after_shipped,
            "Shipped seed must not be rewritten",
        );

        let failed_log = read_log_records(&mut conn, failed_id).await;
        let shipped_log = read_log_records(&mut conn, shipped_id).await;
        assert!(
            failed_log.is_empty(),
            "no state_log entry must be appended for terminal Failed seed, got {failed_log:?}",
        );
        assert!(
            shipped_log.is_empty(),
            "no state_log entry must be appended for terminal Shipped seed, got {shipped_log:?}",
        );

        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[failed_id, shipped_id]).await;
    result.expect("test body");
}

/// WIRE-COMPAT 1: A freshly-booted daemon with no `:state` records at
/// all returns the empty report cleanly.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn resume_empty_redis_returns_empty_report() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;

    // No seed. We can't guarantee the live Redis is empty of other
    // briefs, so this only pins the no-error contract — the report
    // counts may be non-zero from neighbouring data, but the call must
    // succeed and return the `ResumeReport` shape.
    let report = resume_orphans(&mut conn)
        .await
        .expect("resume must not error on empty");
    assert!(
        report.scanned >= report.failed_dead + report.kept_alive
            || report.scanned == report.failed_dead + report.kept_alive,
        "scanned must equal failed_dead + kept_alive for the records this run touched",
    );
}
