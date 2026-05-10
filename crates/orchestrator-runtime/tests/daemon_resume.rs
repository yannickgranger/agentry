//! Integration tests for the boot-time orphan scan (#471a + #471b).
//!
//! Live-Redis like the rest of this crate's redis-integration suite:
//! gate on `AGENTRY_TEST_REDIS_URL` (default `redis://127.0.0.1:6380`)
//! and stay `#[ignore]` so the workspace-wide `cargo test` pass stays
//! green without a Redis dependency.
//!
//! Run with:
//! `cargo test --package orchestrator-runtime --test daemon_resume -- --ignored`

use async_trait::async_trait;
use orchestrator_infra::Config;
use orchestrator_runtime::daemon_resume::{resume_orphans, ResumeReport};
use orchestrator_runtime::lifecycle::{
    EventSource, EventSourceError, StateProjector, StateProjectorError,
};
use orchestrator_types::lifecycle::{
    BriefEvent, BriefState, BriefStateRecord, Reason, RetryBudget,
};
use orchestrator_types::{now, Brief, BriefId, Budget, EscalationMode, VersionedRef};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
        let _: redis::RedisResult<()> = conn.del(format!("agentry:brief:{id}:body")).await;
        let _: redis::RedisResult<()> = conn
            .del(format!("agentry:brief:{id}:state_projector_cursor"))
            .await;
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

async fn seed_body(conn: &mut ConnectionManager, brief_id: &str, topology: &VersionedRef) {
    let brief = Brief {
        id: BriefId(brief_id.into()),
        project: None,
        topology: topology.clone(),
        payload: serde_json::json!({}),
        kind: None,
        contract: None,
        budget: Budget::default(),
        escalation: EscalationMode::default(),
        parent_brief: None,
        cohort_labels: Vec::new(),
        redeploy_required: Vec::new(),
        submitted_by: "test".into(),
        submitted_at: now(),
    };
    let json = serde_json::to_string(&brief).expect("serialize brief");
    let key = format!("agentry:brief:{brief_id}:body");
    let _: () = conn.set(&key, json.as_str()).await.expect("seed body");
}

async fn seed_team(conn: &mut ConnectionManager, name: &str, version: u32) {
    // Minimal-but-valid TeamTopology JSON. `roles: []` is fine for the
    // reattach path — `build_phase_gates` walks an empty list and
    // produces empty `PhaseGates`, which is the only consumer here.
    let team_json = serde_json::json!({
        "name": name,
        "version": version,
        "roles": [],
        "message_graph": [],
        "terminal_role": {"name": "noop", "version": 1},
        "max_retries": 0,
    });
    let key = format!("agentry:team:{name}:v{version}");
    let _: () = conn
        .set(&key, team_json.to_string())
        .await
        .expect("seed team");
}

async fn cleanup_team(conn: &mut ConnectionManager, name: &str, version: u32) {
    let key = format!("agentry:team:{name}:v{version}");
    let _: redis::RedisResult<()> = conn.del(&key).await;
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

/// Test-only `EventSource` that immediately returns `Ok(None)`. The
/// projector_task breaks its loop on `None`, so the spawned task exits
/// cleanly without touching the trace stream — which is precisely what
/// we want here: the contract under test is "reattach calls the
/// factories and spawns the task," not "the task drives the FSM end to
/// end."
struct NoopEventSource;

#[async_trait]
impl EventSource for NoopEventSource {
    async fn next(&mut self) -> Result<Option<BriefEvent>, EventSourceError> {
        Ok(None)
    }
}

/// Test-only `StateProjector` that succeeds without touching Redis.
struct NoopStateProjector;

#[async_trait]
impl StateProjector for NoopStateProjector {
    async fn write(
        &mut self,
        _record: &BriefStateRecord,
        _last_trace_id: &str,
    ) -> Result<(), StateProjectorError> {
        Ok(())
    }
}

type EventFactory = Arc<dyn Fn(BriefId) -> Box<dyn EventSource + Send> + Send + Sync>;
type ProjectorFactory = Arc<dyn Fn(BriefId) -> Box<dyn StateProjector + Send> + Send + Sync>;

/// Counter-bearing factory pair. The returned `Arc`s are the shape the
/// production caller (`orchestratord`) wires; the captured counters let
/// the test assert the factories were invoked the expected number of
/// times.
fn noop_factories() -> (
    EventFactory,
    ProjectorFactory,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let event_calls = Arc::new(AtomicUsize::new(0));
    let projector_calls = Arc::new(AtomicUsize::new(0));
    let event_calls_factory = event_calls.clone();
    let projector_calls_factory = projector_calls.clone();
    let event_factory: EventFactory = Arc::new(move |_brief_id| {
        event_calls_factory.fetch_add(1, Ordering::SeqCst);
        Box::new(NoopEventSource)
    });
    let projector_factory: ProjectorFactory = Arc::new(move |_brief_id| {
        projector_calls_factory.fetch_add(1, Ordering::SeqCst);
        Box::new(NoopStateProjector)
    });
    (
        event_factory,
        projector_factory,
        event_calls,
        projector_calls,
    )
}

/// Best-effort: spawn `podman run -d --rm --name agentry-{agent_id}
/// alpine sleep 60`. Returns `true` if the container is now running and
/// `container_alive` will report it as alive. Returns `false` (with a
/// log line) if podman is unavailable or the spawn failed — the
/// integration test should treat that as a skip.
async fn spawn_sleep_container(agent_id: &str) -> bool {
    let name = format!("agentry-{agent_id}");
    // Pre-clean any stale container under the same name.
    let _ = tokio::process::Command::new("podman")
        .args(["rm", "-f", name.as_str()])
        .output()
        .await;
    let output = tokio::process::Command::new("podman")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            name.as_str(),
            "alpine",
            "sleep",
            "60",
        ])
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            eprintln!(
                "spawn_sleep_container({agent_id}) failed: status={:?} stderr={}",
                o.status,
                String::from_utf8_lossy(&o.stderr)
            );
            false
        }
        Err(e) => {
            eprintln!("spawn_sleep_container({agent_id}) spawn error: {e}");
            false
        }
    }
}

async fn kill_sleep_container(agent_id: &str) {
    let name = format!("agentry-{agent_id}");
    let _ = tokio::process::Command::new("podman")
        .args(["rm", "-f", name.as_str()])
        .output()
        .await;
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

        let (event_factory, projector_factory, _, _) = noop_factories();
        let cfg = Config::default();
        let report = resume_orphans(&mut conn, &event_factory, &projector_factory, &cfg)
            .await
            .expect("resume");
        assert_eq!(
            report,
            ResumeReport {
                scanned: 1,
                failed_dead: 1,
                kept_alive: 0,
                reattach_failed: 0,
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

        let (event_factory, projector_factory, _, _) = noop_factories();
        let cfg = Config::default();
        let report = resume_orphans(&mut conn, &event_factory, &projector_factory, &cfg)
            .await
            .expect("resume");
        assert_eq!(
            report,
            ResumeReport {
                scanned: 0,
                failed_dead: 0,
                kept_alive: 0,
                reattach_failed: 0,
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
    let (event_factory, projector_factory, _, _) = noop_factories();
    let cfg = Config::default();
    let report = resume_orphans(&mut conn, &event_factory, &projector_factory, &cfg)
        .await
        .expect("resume must not error on empty");
    assert!(
        report.scanned
            == report.failed_dead + report.kept_alive + report.reattach_failed,
        "scanned must equal failed_dead + kept_alive + reattach_failed for the records this run touched",
    );
}

/// 471b: a non-terminal brief whose container is alive at scan time
/// triggers the reattach path. The lifecycle driver task is re-spawned
/// (factories invoked once each), the `:state` record is left in its
/// original non-terminal position, and `kept_alive` is bumped.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL) + podman"]
async fn reattach_succeeds_for_live_container() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_brief_reattach_alive_001";
    let agent_id = "agt_reattach_alive_001";
    let team_name = "agentry-reattach-test-team";
    let team_version: u32 = 1;
    let topology = VersionedRef::new(team_name, team_version);

    cleanup(&mut conn, &[id]).await;
    cleanup_team(&mut conn, team_name, team_version).await;

    if !spawn_sleep_container(agent_id).await {
        eprintln!("podman unavailable; skipping reattach_succeeds_for_live_container");
        return;
    }

    let result = async {
        seed_state(
            &mut conn,
            id,
            BriefState::Authoring {
                agent_id: agent_id.into(),
                started_at: now(),
                retry: fresh_retry(),
            },
        )
        .await;
        seed_body(&mut conn, id, &topology).await;
        seed_team(&mut conn, team_name, team_version).await;

        let before = read_state(&mut conn, id).await;

        let (event_factory, projector_factory, event_calls, projector_calls) = noop_factories();
        let cfg = Config::default();
        let report = resume_orphans(&mut conn, &event_factory, &projector_factory, &cfg)
            .await
            .expect("resume");

        assert_eq!(
            report,
            ResumeReport {
                scanned: 1,
                failed_dead: 0,
                kept_alive: 1,
                reattach_failed: 0,
            },
            "live-container record must reattach and bump kept_alive",
        );
        assert_eq!(
            event_calls.load(Ordering::SeqCst),
            1,
            "event_source_factory must be invoked exactly once for the reattached brief",
        );
        assert_eq!(
            projector_calls.load(Ordering::SeqCst),
            1,
            "state_projector_factory must be invoked exactly once for the reattached brief",
        );

        let after = read_state(&mut conn, id).await;
        assert_eq!(
            before.state, after.state,
            "reattach must not rewrite the :state record (still in flight)",
        );

        let log = read_log_records(&mut conn, id).await;
        assert!(
            log.is_empty(),
            "reattach must not append to :state_log; got {log:?}",
        );

        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[id]).await;
    cleanup_team(&mut conn, team_name, team_version).await;
    kill_sleep_container(agent_id).await;
    result.expect("test body");
}

/// 471b: a non-terminal brief whose container is alive at scan time
/// but whose `:body` record is missing falls through to the failed
/// branch — `:state` is rewritten to `Failed{DaemonRestartedDuringExecution}`,
/// `reattach_failed` (NOT `failed_dead`) is bumped, and the container
/// is intentionally not killed (the operator may still want to inspect).
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL) + podman"]
async fn reattach_failure_falls_through_to_failed() {
    let Some(url) = test_redis_url() else {
        return;
    };
    let mut conn = open_conn(&url).await;
    let id = "test_brief_reattach_failure_001";
    let agent_id = "agt_reattach_failure_001";

    cleanup(&mut conn, &[id]).await;

    if !spawn_sleep_container(agent_id).await {
        eprintln!("podman unavailable; skipping reattach_failure_falls_through_to_failed");
        return;
    }

    let result = async {
        seed_state(
            &mut conn,
            id,
            BriefState::Authoring {
                agent_id: agent_id.into(),
                started_at: now(),
                retry: fresh_retry(),
            },
        )
        .await;
        // INTENTIONAL: no seed_body — reattach's body GET will return
        // None and bail with Err, falling through to mark Failed.

        let (event_factory, projector_factory, event_calls, projector_calls) = noop_factories();
        let cfg = Config::default();
        let report = resume_orphans(&mut conn, &event_factory, &projector_factory, &cfg)
            .await
            .expect("resume");

        assert_eq!(
            report,
            ResumeReport {
                scanned: 1,
                failed_dead: 0,
                kept_alive: 0,
                reattach_failed: 1,
            },
            "live-container + missing body must bump reattach_failed (not failed_dead)",
        );
        assert_eq!(
            event_calls.load(Ordering::SeqCst),
            0,
            "event_source_factory must not be invoked when reattach fails before factory call",
        );
        assert_eq!(
            projector_calls.load(Ordering::SeqCst),
            0,
            "state_projector_factory must not be invoked when reattach fails before factory call",
        );

        let after = read_state(&mut conn, id).await;
        assert!(
            matches!(
                after.state,
                BriefState::Failed {
                    reason: Reason::DaemonRestartedDuringExecution
                }
            ),
            "reattach failure must rewrite :state to Failed{{DaemonRestartedDuringExecution}}; got {:?}",
            after.state,
        );

        let log = read_log_records(&mut conn, id).await;
        assert_eq!(
            log.len(),
            1,
            "reattach failure must append exactly one :state_log entry for the Failed transition",
        );

        Ok::<(), String>(())
    }
    .await;

    cleanup(&mut conn, &[id]).await;
    kill_sleep_container(agent_id).await;
    result.expect("test body");
}
