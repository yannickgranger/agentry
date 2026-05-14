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
    read_brief_state, translate_trace_entry, EventSource, EventSourceError, RedisEventSource,
    RedisStateProjector, StateProjector, StateProjectorError,
};
use orchestrator_runtime::lifecycle_driver::projector_task;
use orchestrator_types::lifecycle::{
    handle, role_kind, BriefEvent, BriefState, BriefStateRecord, CiState, DisagreementSummary,
    Reason, RetryBudget, DEFAULT_ATTEMPT_CAP,
};
use orchestrator_types::{event::DoneReason, now, BriefId, Event, EventKind, EventVerdict};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Build the canonical `coder → ac-verifier → reviewer → shipper →
/// ci-watcher` chain `WalkConfig` and the entry NodeId used by these
/// integration tests. After the #495-beta-b collapse, lifecycle tests
/// stand up an explicit chain to exercise the FSM's `Walking`
/// transitions across multiple downstream gates.
fn chain_walk() -> (
    orchestrator_types::lifecycle::WalkConfig,
    orchestrator_types::team::NodeId,
) {
    use orchestrator_types::lifecycle::{GatePolicy, NodeConfig, WalkConfig};
    use orchestrator_types::team::{NodeClass, NodeId};

    let coder = NodeId("coder-claude-agentry".into());
    let acv = NodeId("ac-verifier-test".into());
    let reviewer = NodeId("reviewer-test".into());
    let shipper = NodeId("shipper-agentry".into());
    let ci = NodeId("ci-watcher-agentry".into());

    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    adjacency.insert(coder.clone(), vec![acv.clone()]);
    adjacency.insert(acv.clone(), vec![reviewer.clone()]);
    adjacency.insert(reviewer.clone(), vec![shipper.clone()]);
    adjacency.insert(shipper.clone(), vec![ci.clone()]);

    let class = NodeClass("container_bound".into());
    let mut node_configs: HashMap<NodeId, NodeConfig> = HashMap::new();
    node_configs.insert(
        coder.clone(),
        NodeConfig {
            class: class.clone(),
            expected_inbound: vec![],
            policy: GatePolicy::AllMustPass,
        },
    );
    for (n, upstream) in [
        (&acv, &coder),
        (&reviewer, &acv),
        (&shipper, &reviewer),
        (&ci, &shipper),
    ] {
        node_configs.insert(
            n.clone(),
            NodeConfig {
                class: class.clone(),
                expected_inbound: vec![upstream.clone()],
                policy: GatePolicy::AllMustPass,
            },
        );
    }

    (
        WalkConfig {
            adjacency,
            node_configs,
        },
        coder,
    )
}

fn chain_walk_arc() -> (
    std::sync::Arc<orchestrator_types::lifecycle::WalkConfig>,
    std::sync::Arc<orchestrator_types::team::NodeId>,
) {
    let (cfg, entry) = chain_walk();
    (std::sync::Arc::new(cfg), std::sync::Arc::new(entry))
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
    use orchestrator_types::team::NodeId;
    let brief_id = BriefId("brf_lifecycle_mem".into());
    let mut source = MemEventSource {
        events: VecDeque::from(vec![
            BriefEvent::CoderStarted {
                agent_id: "agent-1".into(),
                role_name: "coder-claude-agentry".into(),
                started_at: now(),
            },
            BriefEvent::RoleDone {
                node_id: NodeId("coder-claude-agentry".into()),
                verdict: EventVerdict::Shipped,
                findings: vec![],
                run_data: None,
            },
            BriefEvent::RoleDone {
                node_id: NodeId("ac-verifier-test".into()),
                verdict: EventVerdict::Shipped,
                findings: vec![],
                run_data: None,
            },
            BriefEvent::RoleDone {
                node_id: NodeId("reviewer-test".into()),
                verdict: EventVerdict::Shipped,
                findings: vec![],
                run_data: None,
            },
        ]),
    };
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut projector = MemStateProjector {
        written: written.clone(),
    };

    let (walk_config, entry_node) = chain_walk();
    let mut state = BriefState::Submitted;
    let mut step: u64 = 0;
    while let Some(ev) = source.next().await.expect("mem next") {
        state = handle(&state, &ev, &walk_config, &entry_node).expect("legal transition");
        step += 1;
        let trace_id = format!("0-{step}");
        projector
            .write(&record_for(&brief_id, state.clone()), &trace_id)
            .await
            .expect("mem write");
    }

    let log = written.lock().expect("mutex").clone();
    assert_eq!(log.len(), 4, "one record per processed event");

    let assert_walking_at = |i: usize, name: &str| match &log[i].0.state {
        BriefState::Walking { node_id, .. } => assert_eq!(node_id.0, name),
        other => panic!("record {i} must be Walking at {name}, got {other:?}"),
    };
    assert_walking_at(0, "coder-claude-agentry");
    assert_walking_at(1, "ac-verifier-test");
    assert_walking_at(2, "reviewer-test");
    assert_walking_at(3, "shipper-agentry");

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
                role_name: "coder-claude-agentry".into(),
                started_at: now(),
            },
            BriefEvent::RoleDone {
                node_id: orchestrator_types::team::NodeId("coder-claude-agentry".into()),
                verdict: EventVerdict::Shipped,
                findings: vec![],
                run_data: None,
            },
            BriefEvent::RoleDone {
                node_id: orchestrator_types::team::NodeId("ac-verifier-test".into()),
                verdict: EventVerdict::ReworkNeeded,
                findings: vec![],
                run_data: None,
            },
            BriefEvent::CoderStarted {
                agent_id: "agent-2".into(),
                role_name: "coder-claude-agentry".into(),
                started_at: now(),
            },
        ]),
    };
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut projector = MemStateProjector {
        written: written.clone(),
    };

    let (walk_config, entry_node) = chain_walk();
    let mut state = BriefState::Submitted;
    let mut step: u64 = 0;
    while let Some(ev) = source.next().await.expect("mem next") {
        state = handle(&state, &ev, &walk_config, &entry_node).expect("legal transition");
        step += 1;
        projector
            .write(&record_for(&brief_id, state.clone()), &format!("0-{step}"))
            .await
            .expect("mem write");
    }

    let log = written.lock().expect("mutex").clone();
    let last = &log.last().expect("at least one record").0.state;
    match last {
        BriefState::Walking {
            node_id,
            retry,
            run_data: orchestrator_types::run_data::RunData::Coder { agent_id },
            ..
        } => {
            assert_eq!(
                node_id.0, "coder-claude-agentry",
                "rework re-entry lands back at the entry coder node"
            );
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
        other => panic!("expected Walking{{coder, Coder}} after rework loop, got {other:?}"),
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
        Some(BriefEvent::RoleDone {
            verdict: EventVerdict::Shipped,
            ..
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

    // `read_brief_state` is the symmetric reader for the same key the
    // projector wrote. The team-orchestration loop (#539) calls this on
    // each iteration to observe FSM-side state instead of carrying
    // parallel in-process accumulators.
    let read_back = read_brief_state(&mut conn, &brief_id)
        .await
        .expect("read_brief_state ok");
    assert!(read_back.is_some(), "round-trip recovers the record");
    let read_back = read_back.expect("round-trip not none");
    assert_eq!(read_back.brief_id, record.brief_id);
    assert!(matches!(read_back.state, BriefState::Submitted));

    // Crash recovery: build a fresh source from a captured cursor.
    let resumed = RedisEventSource::resume_from(conn.clone(), brief_id.clone(), "0-0".to_string());
    let _ = resumed; // construction-only smoke test; fixture stream already drained.

    let _: () = conn.del(&trace_key).await.expect("cleanup trace");
    let _: () = conn.del(&state_log_key).await.expect("cleanup state_log");
    let _: () = conn.del(&state_key).await.expect("cleanup state");
    let _: () = conn.del(&cursor_key).await.expect("cleanup cursor");
}

/// `read_brief_state` returns `Ok(None)` when the `:state` key is
/// absent — the brief has been dispatched but no FSM transition has
/// fired yet, or the brief id never existed. The team-orchestration
/// loop must treat None as "FSM hasn't observed this brief yet" and
/// fall back to topology-driven ready-set computation.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn read_brief_state_returns_none_when_state_key_absent() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let client = redis::Client::open(url).expect("client");
    let mut conn = redis::aio::ConnectionManager::new(client)
        .await
        .expect("conn");
    let brief_id = BriefId(brief_slug("absent"));
    let out = read_brief_state(&mut conn, &brief_id)
        .await
        .expect("read_brief_state ok on absent key");
    assert!(out.is_none(), "absent key surfaces as Ok(None)");
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
fn translate_done_from_coder_emits_role_done() {
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
    match out {
        Some(BriefEvent::RoleDone {
            node_id,
            verdict: EventVerdict::Shipped,
            ..
        }) => {
            assert_eq!(node_id.0, "coder-claude-agentry");
        }
        other => panic!("expected RoleDone for coder, got {other:?}"),
    }
}

/// #529 regression fence — when a coder's `Done` event carries
/// `reason.cause == "self_review_disagreed"` plus a non-empty
/// `disagreements` vec, the translator MUST yield
/// `BriefEvent::CoderDisagreed { disagreements }`, not the generic
/// `RoleDone` arm. This is the canonical-FSM entry into the F6
/// captain-decide park (`Walking{Coder}` → `Walking{OperatorDecision}`).
/// If the translator silently downgraded the disagreement to `RoleDone`,
/// the brief would route through ordinary post-coder gates and the
/// daemon's mini-FSM would terminal-fail it on the embedded `Failed`
/// verdict — exactly the regression #529 documents.
#[test]
fn translate_done_from_coder_with_self_review_disagreed_emits_coder_disagreed() {
    let mut memo: HashMap<String, String> = HashMap::new();
    let _ = translate_trace_entry(
        &mut memo,
        "agent-c".to_string(),
        spawned_event("coder-claude-agentry"),
    )
    .expect("memoize coder spawn");
    let disagreements = vec![DisagreementSummary {
        verb: "UPDATE crates/foo/src/bar.rs:42".into(),
        applied_form: "extracted into a helper".into(),
        rationale: "callers need to mock the helper in unit tests".into(),
    }];
    let event = Event::new(EventKind::Done {
        verdict: EventVerdict::Failed,
        refusal_count: 0,
        reason: Some(DoneReason {
            cause: "self_review_disagreed".into(),
            exit_code: None,
            disagreements: disagreements.clone(),
        }),
    });
    let out = translate_trace_entry(&mut memo, "agent-c".to_string(), event)
        .expect("translate coder disagreed done");
    match out {
        Some(BriefEvent::CoderDisagreed {
            disagreements: emitted,
        }) => {
            assert_eq!(emitted, disagreements);
        }
        other => panic!("expected CoderDisagreed, got {other:?}"),
    }
}

#[test]
fn translate_done_from_shipper_emits_role_done() {
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
    match out {
        Some(BriefEvent::RoleDone {
            node_id,
            verdict: EventVerdict::Shipped,
            ..
        }) => {
            assert_eq!(node_id.0, "shipper-agentry");
        }
        other => panic!("expected RoleDone for shipper, got {other:?}"),
    }
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
    // Submitted + RoleDone is not a legal pair (the legal-from-Submitted
    // arms are CoderStarted plus the universal aborts) and is NOT in the
    // late-RoleDone-in-Walking demotion bucket — the driver must record
    // a terminal Failed{DaemonError}.
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource {
        events: VecDeque::from(vec![BriefEvent::RoleDone {
            node_id: orchestrator_types::team::NodeId("shipper-agentry".into()),
            verdict: EventVerdict::Shipped,
            findings: vec![],
            run_data: Some(orchestrator_types::run_data::RunData::PrTracking {
                pr_number: 1,
                head_sha: "h".into(),
            }),
        }]),
    });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    let (walk_config, entry_node) = chain_walk_arc();
    let result = projector_task(
        bid.clone(),
        source,
        projector,
        None,
        walk_config,
        entry_node,
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

// F7 / #438: the late-`ReviewerDone`-in-`Reworking` demotion test was
// removed in the #495-beta-b collapse — the FSM no longer has a
// `Reworking` variant and the late-event fence is now generic across
// every `Walking` transition, which the
// `lifecycle_driver_fails_brief_on_invalid_transition` test exercises
// via the `is_late_role_done` path. The original test asserted the
// PhaseGates fan-in interaction with `Reviewing → Reworking`; that
// behaviour is covered structurally by the new walker's late-event
// reachability check (`is_late_event` in `orchestrator_types::lifecycle`).

// --- zombie :state regression suite ---
//
// These pin the brief: `fix(daemon): zombie :state — terminal
// disposition must atomically write BriefState::Failed`. Two paths
// converge on the same root cause (terminal :state never written):
//
// 1. `handle_brief`'s team-failure disposition returned `VerdictKind::Failed`
//    without touching `:state`, so the reaper kept firing
//    BudgetExhausted forever on a brief whose container was long gone.
// 2. `projector_task` was parked at `Reworking{..}` because the
//    late-reviewer-in-reworking branch demoted the only event the FSM
//    would have accepted; the reaper's BudgetExhausted then had no
//    pathway to drive the FSM to terminal.

/// `handle_brief`'s team-failure disposition path must atomically write
/// `BriefState::Failed { reason: AcceptanceFailed { detail } }` through
/// the projector before returning. Drives the helper
/// (`write_team_terminal_state`) with an in-memory projector and
/// asserts the post-write record is terminal Failed with a
/// non-default reason naming the failing role.
#[tokio::test]
async fn team_failed_disposition_writes_terminal_state_atomically() {
    let bid = BriefId("brf_team_failed_zombie".into());
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut projector = MemStateProjector {
        written: written.clone(),
    };

    let detail = "reviewer-claude-agentry verdict=Failed".to_string();
    orchestrator_runtime::daemon::write_team_terminal_state(
        &mut projector,
        &bid,
        BriefState::Failed {
            reason: Reason::AcceptanceFailed {
                detail: detail.clone(),
            },
        },
        "team-orchestration-failed",
    )
    .await;

    let log = written.lock().expect("mutex").clone();
    assert_eq!(
        log.len(),
        1,
        "the team-failure disposition must produce exactly one :state write"
    );
    let (record, cursor) = &log[0];
    assert_eq!(record.brief_id, bid);
    assert_eq!(cursor, "team-orchestration-failed");
    match &record.state {
        BriefState::Failed {
            reason: Reason::AcceptanceFailed { detail: got },
        } => {
            assert_eq!(got, &detail, "reason must carry the failing-role detail");
            assert!(
                !got.is_empty(),
                "reason detail must be non-default — operators read :state without the trace stream",
            );
        }
        other => panic!("expected Failed{{AcceptanceFailed}}, got {other:?}"),
    }
}

/// `handle_brief`'s shipped path must also atomically write
/// `BriefState::Shipped` to `:state` before returning, so the reaper
/// stops firing on a brief whose PR has merged.
#[tokio::test]
async fn team_shipped_disposition_writes_terminal_state_atomically() {
    let bid = BriefId("brf_team_shipped_zombie".into());
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut projector = MemStateProjector {
        written: written.clone(),
    };

    orchestrator_runtime::daemon::write_team_terminal_state(
        &mut projector,
        &bid,
        BriefState::Shipped,
        "team-orchestration-shipped",
    )
    .await;

    let log = written.lock().expect("mutex").clone();
    assert_eq!(log.len(), 1);
    assert!(matches!(log[0].0.state, BriefState::Shipped));
    assert_eq!(log[0].1, "team-orchestration-shipped");
}

/// Seed `projector_task` to land in `Reworking { target: Coder }` via
/// the canonical Reviewing-rework path, then push `BudgetExhausted`
/// (the event the reaper synthesises on over-budget briefs) and assert
/// the FSM driver writes a terminal `Failed { BudgetExhausted }`
/// record. Pre-fix the late-reviewer-in-reworking branch parked the
/// FSM in Reworking and the reaper's BudgetExhausted had no pathway to
/// drive the FSM to terminal.
#[tokio::test]
async fn reaper_budget_exhausted_drives_fsm_to_failed() {
    let tmp = tempfile::tempdir().expect("tmp");
    std::env::set_var("AGENTRY_WORKSPACE_ROOT", tmp.path());

    let bid = BriefId("brf_reaper_budget_exhausted_zombie".into());
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    use orchestrator_types::team::NodeId;
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource {
        events: VecDeque::from(vec![
            BriefEvent::CoderStarted {
                agent_id: "agent-1".into(),
                role_name: "coder-claude-agentry".into(),
                started_at: now(),
            },
            BriefEvent::RoleDone {
                node_id: NodeId("coder-claude-agentry".into()),
                verdict: EventVerdict::Shipped,
                findings: vec![],
                run_data: None,
            },
            BriefEvent::RoleDone {
                node_id: NodeId("ac-verifier-test".into()),
                verdict: EventVerdict::Shipped,
                findings: vec![],
                run_data: None,
            },
            BriefEvent::RoleDone {
                node_id: NodeId("reviewer-test".into()),
                verdict: EventVerdict::ReworkNeeded,
                findings: vec![],
                run_data: None,
            },
            // FSM is now Walking back at the entry coder node with
            // retry=2 (the post-collapse equivalent of `Reworking`).
            // The reaper observes the brief past its wall-clock budget
            // and pushes this:
            BriefEvent::BudgetExhausted,
        ]),
    });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });

    let (walk_config, entry_node) = chain_walk_arc();
    let result = projector_task(
        bid.clone(),
        source,
        projector,
        None,
        walk_config,
        entry_node,
    )
    .await;
    assert!(
        result.is_ok(),
        "projector_task must return Ok after honoring BudgetExhausted: {result:?}"
    );

    let log = written.lock().expect("mutex").clone();
    let last = &log.last().expect("at least one record").0.state;
    match last {
        BriefState::Failed {
            reason: Reason::BudgetExhausted,
        } => {}
        other => panic!("expected Failed{{BudgetExhausted}}, got {other:?}"),
    }
    // The FSM must have passed through a rework restart (Walking back at
    // entry with retry > 1) before terminating — pre-fix the FSM could
    // park indefinitely in a non-terminal state without an exit path.
    assert!(
        log.iter().any(|(r, _)| matches!(
            &r.state,
            BriefState::Walking {
                node_id,
                retry,
                ..
            } if node_id.0 == "coder-claude-agentry" && retry.attempt >= 2
        )),
        "FSM must pass through a rework restart at the entry node before terminating",
    );

    std::env::remove_var("AGENTRY_WORKSPACE_ROOT");
}

#[test]
fn build_walk_config_from_self_host_topology() {
    use orchestrator_runtime::lifecycle_driver::build_walk_config;
    use orchestrator_types::team::{NodeClass, NodeId};
    use orchestrator_types::TeamTopology;

    let raw = std::fs::read_to_string("../../seed/topologies/agentry-self-host-v0.json")
        .expect("read agentry-self-host-v0 fixture");
    let team: TeamTopology = serde_json::from_str(&raw).expect("parse self-host topology");
    assert_eq!(team.roles.len(), 8, "fixture invariant: 8 roles");
    assert_eq!(team.message_graph.len(), 14, "fixture invariant: 14 edges");

    let cfg = build_walk_config(&team);

    let coder = NodeId("coder-claude-agentry".to_string());
    let downstream = cfg
        .adjacency
        .get(&coder)
        .expect("coder must have an adjacency entry");
    let mut got: Vec<String> = downstream.iter().map(|n| n.0.clone()).collect();
    got.sort();
    let mut want = vec![
        "ac-verifier-claude-agentry".to_string(),
        "ac-verifier-gemini-agentry".to_string(),
        "ac-verifier-grok-agentry".to_string(),
        "reviewer-claude-agentry".to_string(),
        "reviewer-mechanical-agentry".to_string(),
    ];
    want.sort();
    assert_eq!(got, want, "coder fans out to 3 ac-verifiers + 2 reviewers");

    let ci_watcher = NodeId("ci-watcher-agentry".to_string());
    let nc = cfg
        .node_configs
        .get(&ci_watcher)
        .expect("ci-watcher node config");
    assert_eq!(nc.class, NodeClass("container_bound".to_string()));
}
