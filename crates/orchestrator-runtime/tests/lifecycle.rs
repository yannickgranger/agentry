//! Integration tests for the brief-lifecycle Redis adapters.
//!
//! Two layers:
//!
//! * In-memory adapters (`MemEventSource`, `MemStateProjector`) drive
//!   `orchestrator_types::lifecycle::handle` end-to-end without any
//!   Redis dependency. These run on every `cargo test --workspace`.
//! * Live Redis tests (`#[ignore = "requires AGENTRY_TEST_REDIS_URL"]`)
//!   exercise the production `RedisEventSource` /
//!   `RedisStateProjector` against a real Redis. Skipped in CI per the
//!   existing `redis_io_test.rs` pattern.

use async_trait::async_trait;
use chrono::Utc;
use orchestrator_runtime::lifecycle::{
    EventSource, EventSourceError, RedisEventSource, RedisStateProjector, StateProjector,
    StateProjectorError,
};
use orchestrator_types::lifecycle::{
    handle, BriefEvent, BriefState, BriefStateRecord, RetryBudget, DEFAULT_ATTEMPT_CAP,
};
use orchestrator_types::{BriefId, EventVerdict};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

struct MemEventSource {
    events: VecDeque<BriefEvent>,
}

#[async_trait]
impl EventSource for MemEventSource {
    async fn next(&mut self) -> Result<Option<BriefEvent>, EventSourceError> {
        Ok(self.events.pop_front())
    }
}

struct MemStateProjector {
    written: Arc<Mutex<Vec<(BriefStateRecord, String)>>>,
}

#[async_trait]
impl StateProjector for MemStateProjector {
    async fn write(
        &mut self,
        record: &BriefStateRecord,
        last_trace_id: &str,
    ) -> Result<(), StateProjectorError> {
        self.written
            .lock()
            .expect("MemStateProjector mutex poisoned")
            .push((record.clone(), last_trace_id.to_string()));
        Ok(())
    }
}

fn record_for(brief_id: &BriefId, state: BriefState) -> BriefStateRecord {
    BriefStateRecord {
        brief_id: brief_id.clone(),
        state,
        parent_brief_id: None,
        composition_role: None,
        at: Utc::now(),
    }
}

/// Drive a fixture event stream through `handle()` via the in-memory
/// adapters. Asserts the projector observes the FSM-correct sequence
/// of states (Submitted → Authoring → Verifying → Reviewing →
/// Shipping) and that each write carries the synthetic trace id.
#[tokio::test]
async fn mem_adapters_drive_handle() {
    let brief_id = BriefId("brf_lifecycle_mem".into());
    let mut source = MemEventSource {
        events: VecDeque::from(vec![
            BriefEvent::CoderStarted {
                agent_id: "agent-1".into(),
            },
            BriefEvent::CoderDone {
                verdict: EventVerdict::Shipped,
            },
            BriefEvent::AcVerifierDone {
                verdict: EventVerdict::Shipped,
            },
            BriefEvent::ReviewerDone {
                verdict: EventVerdict::Shipped,
                findings: vec![],
            },
        ]),
    };
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut projector = MemStateProjector {
        written: written.clone(),
    };

    let mut state = BriefState::Submitted;
    let mut step: u64 = 0;
    while let Some(ev) = source.next().await.expect("mem next") {
        state = handle(&state, &ev).expect("legal transition");
        step += 1;
        let trace_id = format!("0-{step}");
        projector
            .write(&record_for(&brief_id, state.clone()), &trace_id)
            .await
            .expect("mem write");
    }

    let log = written.lock().expect("mutex").clone();
    assert_eq!(log.len(), 4, "one record per processed event");

    assert!(matches!(log[0].0.state, BriefState::Authoring { .. }));
    assert!(matches!(log[1].0.state, BriefState::Verifying { .. }));
    assert!(matches!(log[2].0.state, BriefState::Reviewing { .. }));
    assert!(matches!(log[3].0.state, BriefState::Shipping { .. }));

    assert_eq!(log[3].1, "0-4");
    for (record, _) in &log {
        assert_eq!(record.brief_id.0, "brf_lifecycle_mem");
    }
}

/// Verify a rework loop: an ac-verifier failure bumps the retry budget
/// and the projector observes the `Reworking` record before the next
/// `CoderStarted` returns the brief to `Authoring` with the bumped
/// counter intact.
#[tokio::test]
async fn mem_adapters_carry_retry_budget_through_rework() {
    let brief_id = BriefId("brf_lifecycle_rework".into());
    let mut source = MemEventSource {
        events: VecDeque::from(vec![
            BriefEvent::CoderStarted {
                agent_id: "agent-1".into(),
            },
            BriefEvent::CoderDone {
                verdict: EventVerdict::Shipped,
            },
            BriefEvent::AcVerifierDone {
                verdict: EventVerdict::ReworkNeeded,
            },
            BriefEvent::CoderStarted {
                agent_id: "agent-2".into(),
            },
        ]),
    };
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut projector = MemStateProjector {
        written: written.clone(),
    };

    let mut state = BriefState::Submitted;
    let mut step: u64 = 0;
    while let Some(ev) = source.next().await.expect("mem next") {
        state = handle(&state, &ev).expect("legal transition");
        step += 1;
        projector
            .write(&record_for(&brief_id, state.clone()), &format!("0-{step}"))
            .await
            .expect("mem write");
    }

    let log = written.lock().expect("mutex").clone();
    let last = &log.last().expect("at least one record").0.state;
    match last {
        BriefState::Authoring {
            retry, agent_id, ..
        } => {
            assert_eq!(agent_id, "agent-2", "rework re-entry pins the new agent id");
            assert_eq!(
                *retry,
                RetryBudget {
                    attempt: 2,
                    max: DEFAULT_ATTEMPT_CAP
                },
                "rework loop bumps the attempt counter"
            );
        }
        other => panic!("expected Authoring after rework loop, got {other:?}"),
    }
}

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn brief_slug(prefix: &str) -> String {
    format!(
        "brf_lifecycle_{prefix}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// Live-Redis round-trip: seed a trace stream with a `coder` spawn +
/// `Done(Shipped)` event, drive `RedisEventSource` through one
/// translation, then `RedisStateProjector::write` the resulting record.
/// Verifies the three keys (state_log XADD, state SET, cursor SET) all
/// land via the Lua atomic write.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn redis_round_trip_writes_three_keys_atomically() {
    use redis::AsyncCommands;

    let Some(url) = test_redis_url() else {
        return;
    };
    let client = redis::Client::open(url).expect("client");
    let mut conn = redis::aio::ConnectionManager::new(client)
        .await
        .expect("conn");

    let id_str = brief_slug("rt");
    let brief_id = BriefId(id_str.clone());
    let trace_key = format!("agentry:brief:{id_str}:trace");
    let state_log_key = format!("agentry:brief:{id_str}:state_log");
    let state_key = format!("agentry:brief:{id_str}:state");
    let cursor_key = format!("agentry:brief:{id_str}:state_projector_cursor");

    let spawned_event = serde_json::json!({
        "at": Utc::now().to_rfc3339(),
        "type": "event",
        "payload": { "agent_event": "spawned", "role_name": "coder" }
    })
    .to_string();
    let done_event = serde_json::json!({
        "at": Utc::now().to_rfc3339(),
        "type": "done",
        "verdict": "shipped"
    })
    .to_string();

    let _: String = conn
        .xadd(
            &trace_key,
            "*",
            &[("agent", "agent-rt"), ("event", spawned_event.as_str())],
        )
        .await
        .expect("xadd spawned");
    let _: String = conn
        .xadd(
            &trace_key,
            "*",
            &[("agent", "agent-rt"), ("event", done_event.as_str())],
        )
        .await
        .expect("xadd done");

    let mut source = RedisEventSource::new(conn.clone(), brief_id.clone());
    let started = source.next().await.expect("started");
    assert!(matches!(started, Some(BriefEvent::CoderStarted { .. })));
    let coder_done = source.next().await.expect("done");
    assert!(matches!(
        coder_done,
        Some(BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped
        })
    ));

    let mut projector = RedisStateProjector::new(conn.clone(), brief_id.clone());
    let record = record_for(&brief_id, BriefState::Submitted);
    projector
        .write(&record, "fixture-trace-id")
        .await
        .expect("write");

    let log_len: i64 = conn.xlen(&state_log_key).await.expect("xlen");
    assert_eq!(log_len, 1, "state_log gets one XADD per write");
    let state_blob: Option<String> = conn.get(&state_key).await.expect("get state");
    assert!(state_blob.is_some(), "state key populated");
    let cursor: Option<String> = conn.get(&cursor_key).await.expect("get cursor");
    assert_eq!(cursor.as_deref(), Some("fixture-trace-id"));

    // Crash recovery: build a fresh source from a captured cursor.
    let resumed = RedisEventSource::resume_from(conn.clone(), brief_id.clone(), "0-0".to_string());
    let _ = resumed; // construction-only smoke test; fixture stream already drained.

    let _: () = conn.del(&trace_key).await.expect("cleanup trace");
    let _: () = conn.del(&state_log_key).await.expect("cleanup state_log");
    let _: () = conn.del(&state_key).await.expect("cleanup state");
    let _: () = conn.del(&cursor_key).await.expect("cleanup cursor");
}

/// Live-Redis: a trace-stream entry of the shape
/// `{"type":"retry_requested","actor":"...","reason":"..."}` is decoded
/// by `RedisEventSource::next` into `BriefEvent::RetryRequested {actor,
/// reason}`. The producer (operator CLI / dashboard / external script)
/// is out of scope; this test only pins the EventSource decoding rule.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn redis_event_source_translates_retry_requested() {
    use redis::AsyncCommands;

    let Some(url) = test_redis_url() else {
        return;
    };
    let client = redis::Client::open(url).expect("client");
    let mut conn = redis::aio::ConnectionManager::new(client)
        .await
        .expect("conn");

    let id_str = brief_slug("retry_requested");
    let brief_id = BriefId(id_str.clone());
    let trace_key = format!("agentry:brief:{id_str}:trace");

    let entry = serde_json::json!({
        "at": Utc::now().to_rfc3339(),
        "type": "retry_requested",
        "actor": "alice",
        "reason": "flake on CI"
    })
    .to_string();
    let _: String = conn
        .xadd(
            &trace_key,
            "*",
            &[("agent", "operator-cli"), ("event", entry.as_str())],
        )
        .await
        .expect("xadd retry_requested");

    let mut source = RedisEventSource::new(conn.clone(), brief_id.clone());
    match source.next().await.expect("next") {
        Some(BriefEvent::RetryRequested { actor, reason }) => {
            assert_eq!(actor, "alice");
            assert_eq!(reason, "flake on CI");
        }
        other => panic!("expected BriefEvent::RetryRequested, got {other:?}"),
    }

    let _: () = conn.del(&trace_key).await.expect("cleanup trace");
}
