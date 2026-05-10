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
    translate_trace_entry, EventSource, EventSourceError, RedisEventSource, RedisStateProjector,
    StateProjector, StateProjectorError,
};
use orchestrator_runtime::lifecycle_driver::projector_task;
use orchestrator_types::lifecycle::{
    handle, role_kind, BriefEvent, BriefState, BriefStateRecord, CiState, Reason, RetryBudget,
    DEFAULT_ATTEMPT_CAP,
};
use orchestrator_types::{now, BriefId, Event, EventKind, EventVerdict};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

fn no_gates() -> orchestrator_types::lifecycle::PhaseGates {
    use orchestrator_types::lifecycle::{GateConfig, GatePolicy, PhaseGates};
    // Non-empty expected roles: with the E/1 empty-phase auto-skip,
    // empty/empty would short-circuit Authoring → Shipping and the
    // multi-step trail asserted by these tests would be unreachable.
    PhaseGates {
        verifying: GateConfig {
            expected_roles: vec!["ac-verifier-test".to_owned()],
            policy: GatePolicy::AllMustPass,
        },
        reviewing: GateConfig {
            expected_roles: vec!["reviewer-test".to_owned()],
            policy: GatePolicy::AllMustPass,
        },
    }
}

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
                started_at: now(),
            },
            BriefEvent::CoderDone {
                verdict: EventVerdict::Shipped,
            },
            BriefEvent::AcVerifierDone {
                verdict: EventVerdict::Shipped,
                role_name: "ac-verifier-test".to_owned(),
            },
            BriefEvent::ReviewerDone {
                verdict: EventVerdict::Shipped,
                findings: vec![],
                role_name: "reviewer-test".to_owned(),
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
        state = handle(&state, &ev, &no_gates()).expect("legal transition");
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
                started_at: now(),
            },
            BriefEvent::CoderDone {
                verdict: EventVerdict::Shipped,
            },
            BriefEvent::AcVerifierDone {
                verdict: EventVerdict::ReworkNeeded,
                role_name: "ac-verifier-test".to_owned(),
            },
            BriefEvent::CoderStarted {
                agent_id: "agent-2".into(),
                started_at: now(),
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
        state = handle(&state, &ev, &no_gates()).expect("legal transition");
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

// --- role-kind normalization + translate_trace_entry unit tests ---
//
// These pin the brief-#392 fix: `role_by_agent` memoizes the SHORT
// role kind (so the `Done` branch's `match role.as_deref()` arms
// trigger), and the `Done` branch grew arms for `shipper-agentry`
// and `ci-watcher-agentry` so the FSM actually advances through
// Shipping → Watching → Shipped.

fn spawned_event(role_name: &str) -> Event {
    Event::new(EventKind::Event {
        payload: serde_json::json!({
            "agent_event": "spawned",
            "role_name": role_name,
        }),
    })
}

fn done_event(verdict: EventVerdict) -> Event {
    Event::new(EventKind::Done {
        verdict,
        reason: None,
        refusal_count: 0,
    })
}

#[test]
fn role_kind_maps_each_family_to_its_short_kind() {
    assert_eq!(role_kind("coder-claude-agentry"), Some("coder"));
    assert_eq!(role_kind("coder-grok-agentry"), Some("coder"));
    assert_eq!(role_kind("ac-verifier-agentry"), Some("ac-verifier"));
    assert_eq!(role_kind("verifier-agentry"), Some("verifier"));
    assert_eq!(role_kind("reviewer-agentry"), Some("reviewer"));
    assert_eq!(role_kind("shipper-agentry"), Some("shipper"));
    assert_eq!(role_kind("ci-watcher-agentry"), Some("ci-watcher"));
    assert_eq!(role_kind("preflight-criterion-agentry"), Some("preflight"));
    assert_eq!(role_kind("unknown-role"), None);
    // `coder` without the family-suffix is NOT a recognised coder
    // role — the spawner always emits the suffixed form. Pre-fix the
    // translator memoized this bare string and the `match Some("coder")`
    // arm matched it; after the fix the prefix-only match guarantees
    // the spawner-shape is what triggers the arm.
    assert_eq!(role_kind("coder"), None);
}

#[test]
fn translate_spawned_coder_claude_agentry_emits_coder_started() {
    let mut memo: HashMap<String, String> = HashMap::new();
    let out = translate_trace_entry(
        &mut memo,
        "agent-99".to_string(),
        spawned_event("coder-claude-agentry"),
    )
    .expect("translate spawned coder-claude-agentry");
    match out {
        Some(BriefEvent::CoderStarted { agent_id, .. }) => {
            assert_eq!(agent_id, "agent-99");
        }
        other => panic!("expected CoderStarted, got {other:?}"),
    }
    assert_eq!(
        memo.get("agent-99").map(String::as_str),
        Some("coder-claude-agentry")
    );
}

#[test]
fn translate_done_from_coder_emits_coder_done() {
    let mut memo: HashMap<String, String> = HashMap::new();
    let _ = translate_trace_entry(
        &mut memo,
        "agent-c".to_string(),
        spawned_event("coder-claude-agentry"),
    )
    .expect("memoize coder spawn");
    let out = translate_trace_entry(
        &mut memo,
        "agent-c".to_string(),
        done_event(EventVerdict::Shipped),
    )
    .expect("translate coder done");
    assert!(matches!(
        out,
        Some(BriefEvent::CoderDone {
            verdict: EventVerdict::Shipped
        })
    ));
}

#[test]
fn translate_done_from_shipper_emits_shipper_done() {
    let mut memo: HashMap<String, String> = HashMap::new();
    let _ = translate_trace_entry(
        &mut memo,
        "agent-s".to_string(),
        spawned_event("shipper-agentry"),
    )
    .expect("memoize shipper spawn");
    assert_eq!(
        memo.get("agent-s").map(String::as_str),
        Some("shipper-agentry")
    );
    let out = translate_trace_entry(
        &mut memo,
        "agent-s".to_string(),
        done_event(EventVerdict::Shipped),
    )
    .expect("translate shipper done");
    assert!(matches!(out, Some(BriefEvent::ShipperDone { .. })));
}

#[test]
fn translate_done_from_ci_watcher_maps_verdict_to_ci_state() {
    let cases = [
        (EventVerdict::Shipped, CiState::Success),
        (EventVerdict::Failed, CiState::Failed),
        (EventVerdict::Escalated, CiState::Pending),
        (EventVerdict::ReworkNeeded, CiState::Pending),
        (EventVerdict::Rejected, CiState::Pending),
    ];
    for (verdict, expected) in cases {
        let mut memo: HashMap<String, String> = HashMap::new();
        let _ = translate_trace_entry(
            &mut memo,
            "agent-w".to_string(),
            spawned_event("ci-watcher-agentry"),
        )
        .expect("memoize ci-watcher spawn");
        let out = translate_trace_entry(&mut memo, "agent-w".to_string(), done_event(verdict))
            .expect("translate ci-watcher done");
        match out {
            Some(BriefEvent::CiResult { state, .. }) => {
                assert_eq!(state, expected, "verdict {verdict:?} → {expected:?}");
            }
            other => panic!("expected CiResult for {verdict:?}, got {other:?}"),
        }
    }
}

// --- 396b-3: lifecycle_driver fails the brief on InvalidTransition ---

/// Drive `projector_task` with an event sequence that the FSM rejects
/// (start at `Submitted`, then yield `ShipperDone` — there is no
/// `Submitted + ShipperDone` arm). The driver must translate the
/// `InvalidTransition` into a written `BriefStateRecord` carrying
/// `Failed{DaemonError}` whose detail mentions the rejected event,
/// then break out of its loop returning `Ok(())`.
#[tokio::test]
async fn lifecycle_driver_fails_brief_on_invalid_transition() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::env::set_var("AGENTRY_WORKSPACE_ROOT", tmp.path());

    let bid = BriefId("brf_invalid_transition_fence".into());
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource {
        events: VecDeque::from(vec![BriefEvent::ShipperDone {
            pr_number: 1,
            head_sha: "h".into(),
        }]),
    });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    let result = projector_task(
        bid.clone(),
        source,
        projector,
        None,
        std::sync::Arc::new(no_gates()),
    )
    .await;
    assert!(
        result.is_ok(),
        "projector_task must return Ok after fencing on InvalidTransition: {result:?}"
    );

    let log = written.lock().expect("mutex").clone();
    assert_eq!(
        log.len(),
        1,
        "one record written for the InvalidTransition fence"
    );
    match &log[0].0.state {
        BriefState::Failed {
            reason: Reason::DaemonError { detail },
        } => {
            assert!(
                detail.contains("FSM rejected"),
                "detail must mention the FSM rejection: {detail}"
            );
        }
        other => panic!("expected Failed{{DaemonError}}, got {other:?}"),
    }

    std::env::remove_var("AGENTRY_WORKSPACE_ROOT");
}

/// F7 / #438: when reviewer-claude finishes first with `ReworkNeeded`
/// the FSM lands in `Reworking { target: Coder }`. A second reviewer's
/// later `ReviewerDone` is illegal in that state — Option C demotes
/// that one rejection from a brief-killing `DaemonError` to a
/// `tracing::warn!` and a continue. Other rejections stay terminal
/// (covered by `lifecycle_driver_fails_brief_on_invalid_transition`).
///
/// This test drives `projector_task` to `Reworking { target: Coder }`
/// via the canonical Reviewing-rework path, then yields one more
/// `ReviewerDone { Shipped }` and asserts the brief stays in
/// `Reworking` and no terminal record is written.
#[tokio::test]
async fn late_reviewer_done_in_reworking_is_dropped_not_failed() {
    let bid = BriefId("brf_late_reviewer_in_reworking".into());
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource {
        events: VecDeque::from(vec![
            BriefEvent::CoderStarted {
                agent_id: "agent-1".into(),
                started_at: now(),
            },
            BriefEvent::CoderDone {
                verdict: EventVerdict::Shipped,
            },
            BriefEvent::AcVerifierDone {
                verdict: EventVerdict::Shipped,
                role_name: "ac-verifier-test".to_owned(),
            },
            BriefEvent::ReviewerDone {
                verdict: EventVerdict::ReworkNeeded,
                findings: vec![],
                role_name: "reviewer-test".to_owned(),
            },
            // Late ReviewerDone from a second reviewer arriving after
            // the FSM has already left Reviewing for Reworking. The
            // FSM rejects this; the driver must warn-and-drop.
            BriefEvent::ReviewerDone {
                verdict: EventVerdict::Shipped,
                findings: vec![],
                role_name: "reviewer-mechanical-test".to_owned(),
            },
        ]),
    });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    let result = projector_task(
        bid.clone(),
        source,
        projector,
        None,
        std::sync::Arc::new(no_gates()),
    )
    .await;
    assert!(
        result.is_ok(),
        "projector_task must return Ok when the late ReviewerDone is dropped: {result:?}"
    );

    let log = written.lock().expect("mutex").clone();
    // Four valid transitions: Authoring, Verifying, Reviewing,
    // Reworking. The fifth event is dropped without writing a record.
    assert_eq!(
        log.len(),
        4,
        "late ReviewerDone in Reworking must not produce a state record"
    );
    match &log.last().expect("at least one record").0.state {
        BriefState::Reworking { .. } => {}
        other => panic!("expected last state Reworking, got {other:?}"),
    }
    for (record, _) in &log {
        assert!(
            !matches!(
                record.state,
                BriefState::Failed { .. } | BriefState::Shipped
            ),
            "no terminal record should be written: {:?}",
            record.state
        );
    }
}
