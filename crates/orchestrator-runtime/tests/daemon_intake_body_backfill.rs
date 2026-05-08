//! Integration test for the daemon's stream-intake body-key backfill.
//!
//! `redis_io::submit_brief` writes `agentry:brief:<id>:body` on the API
//! path so the dashboard's SMEMBERS+MGET render can find a body to
//! display. Direct-XADD callers (operator tooling, captain scripts,
//! replay/recovery) bypass that SET and the dashboard renders 'No
//! briefs in flight' even though the daemon is processing the brief
//! correctly — that's the bug this test pins.
//!
//! The fix moves the body-key write into the daemon's intake loop via
//! the `backfill_body_key` helper. The test below exercises that
//! helper directly against live Redis (matching the
//! `tests/daemon_test.rs` and `tests/redis_io_test.rs` pattern). The
//! call site in `daemon::run` is verified by reading the source —
//! end-to-end stream-intake spawns containers and is out of scope for
//! a unit test.
//!
//! Live-Redis: gates on `AGENTRY_TEST_REDIS_URL`, stays `#[ignore]` so
//! the workspace-wide `cargo test` pass stays green without a Redis
//! dependency.

use chrono::Utc;
use orchestrator_runtime::daemon::backfill_body_key;
use orchestrator_runtime::redis_io::connect;
use orchestrator_types::{Brief, BriefId, Budget, EscalationMode, VersionedRef};
use redis::AsyncCommands;

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn brief_slug() -> String {
    format!(
        "brf_test_backfill_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn make_brief(id: &str) -> Brief {
    Brief {
        id: BriefId(id.into()),
        project: None,
        topology: VersionedRef::new("zz-test-topo", 1),
        payload: serde_json::json!({"hello": "world"}),
        kind: None,
        contract: None,
        budget: Budget::default(),
        escalation: EscalationMode::default(),
        parent_brief: None,
        cohort_labels: vec![],
        submitted_by: "test".into(),
        submitted_at: Utc::now(),
    }
}

/// Raw-XADD intake path: a brief lands on `agentry:briefs` without
/// `submit_brief` having run, so `agentry:brief:<id>:body` is unset.
/// `backfill_body_key` (the helper the daemon's intake loop calls
/// before spawning) writes the JSON, so the dashboard's render
/// SMEMBERS+MGET finds the body.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn backfill_writes_body_key_for_raw_xadd_intake() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let id = brief_slug();
    let body_key = format!("agentry:brief:{id}:body");

    // Simulate the raw-XADD path: the body key is NOT pre-set because
    // `submit_brief` was bypassed.
    let _: () = conn.del(&body_key).await.expect("pre-clean");
    let pre: Option<String> = conn.get(&body_key).await.expect("pre-get");
    assert!(
        pre.is_none(),
        "body key must be unset before backfill (raw-XADD scenario)"
    );

    let brief = make_brief(&id);
    backfill_body_key(&mut conn, &brief)
        .await
        .expect("backfill must succeed");

    let raw: Option<String> = conn.get(&body_key).await.expect("get");
    let raw = raw.expect("backfill must set body key");
    let back: Brief = serde_json::from_str(&raw).expect("parse");
    assert_eq!(back.id.0, id, "round-tripped body must match input id");
    assert_eq!(back, brief, "round-tripped body must equal input brief");

    let _: () = conn.del(&body_key).await.expect("cleanup");
}

/// Idempotency contract: the daemon's intake-loop backfill runs on
/// every brief (including those that `submit_brief` already pre-wrote
/// on the API path). A second `backfill_body_key` for the same brief
/// must overwrite without error and leave a parseable body behind.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn backfill_is_idempotent_with_submit_brief() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let id = brief_slug();
    let body_key = format!("agentry:brief:{id}:body");

    let brief = make_brief(&id);

    // Pre-write to mimic what `submit_brief` does on the API path.
    let pre_body = serde_json::to_string(&brief).expect("serialize");
    let _: () = conn
        .set(&body_key, pre_body.as_str())
        .await
        .expect("pre-write");

    // Daemon backfill runs after — must not error and must leave a
    // valid body in place.
    backfill_body_key(&mut conn, &brief)
        .await
        .expect("idempotent backfill");

    let raw: Option<String> = conn.get(&body_key).await.expect("get");
    let raw = raw.expect("body key still present");
    let back: Brief = serde_json::from_str(&raw).expect("parse");
    assert_eq!(back, brief, "idempotent overwrite preserves brief content");

    let _: () = conn.del(&body_key).await.expect("cleanup");
}
