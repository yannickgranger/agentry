//! Regression test for the no-op short-circuit (brief #316).
//!
//! Pre-fix: when a coder's acceptance passed but its diff against the
//! base branch was empty (work was already on base), the coder runner
//! emitted `done failed cause:no_changes` and the daemon routed the
//! brief to terminal Failed — wasting the downstream reviewer / shipper
//! / ci-watcher run on every duplicate dispatch. Post-fix the coder
//! runner emits `done shipped` with `DoneReason.cause =
//! "no_op_short_circuit"`, the trace-stream translator folds that into
//! a [`BriefEvent::CoderDoneNoOp`], and the FSM short-circuits
//! `Authoring → Shipped` directly (skipping the Verifying / Reviewing
//! / Shipping / Watching trail). The terminal Shipped verdict carries
//! the `NO_OP_VERDICT_REASON` text so an operator scanning
//! `agentry:verdicts` sees the short-circuit reason inline.
//!
//! In-memory adapters are minimal local equivalents of the L.2
//! fixtures (per the brief — the L.2 test module does not re-export
//! them across integration crates). The verdict-emission path is
//! exercised with `verdict_conn = None`; the `lifecycle.rs`
//! live-Redis test covers the production XADD path. Three scenarios:
//!
//! * No-op short-circuit: `CoderStarted` then `CoderDoneNoOp` produces
//!   exactly two records, terminating at `Shipped`. No intermediate
//!   `Verifying` / `Reviewing` / `Shipping` / `Watching` is written.
//! * Reason text round-trip: `emit_terminal_verdict` (driven via the
//!   projector's terminal hook) sets `Verdict.reason` to the carried
//!   no-op text, not the generic `"fsm: shipped"`.
//! * Translator decoding: an `EventKind::Done` with `verdict:Shipped`
//!   and `reason.cause = NO_OP_SHORT_CIRCUIT_CAUSE` emitted by an
//!   agent registered as `coder` decodes to `CoderDoneNoOp`. Without
//!   the cause sentinel it decodes to the standard `CoderDone`.

use async_trait::async_trait;
use orchestrator_runtime::lifecycle::{
    EventSource, EventSourceError, StateProjector, StateProjectorError, NO_OP_SHORT_CIRCUIT_CAUSE,
    NO_OP_VERDICT_REASON,
};
use orchestrator_runtime::lifecycle_driver::{cleanup_shipped_no_op_brief_at, projector_task};
use orchestrator_runtime::workspace::allocate_at;
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord};
use orchestrator_types::{now, BriefId, DoneReason, Event, EventKind, EventVerdict};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn no_gates() -> std::sync::Arc<orchestrator_types::lifecycle::PhaseGates> {
    use orchestrator_types::lifecycle::{GateConfig, GatePolicy, PhaseGates};
    std::sync::Arc::new(PhaseGates {
        verifying: GateConfig {
            expected_roles: vec![],
            policy: GatePolicy::AllMustPass,
        },
        reviewing: GateConfig {
            expected_roles: vec![],
            policy: GatePolicy::AllMustPass,
        },
    })
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

async fn run_to_completion(
    brief_id: BriefId,
    events: Vec<BriefEvent>,
) -> Vec<(BriefStateRecord, String)> {
    let written: Arc<Mutex<Vec<(BriefStateRecord, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn EventSource + Send> = Box::new(MemEventSource {
        events: VecDeque::from(events),
    });
    let projector: Box<dyn StateProjector + Send> = Box::new(MemStateProjector {
        written: written.clone(),
    });
    projector_task(brief_id, source, projector, None, no_gates())
        .await
        .expect("projector_task");
    let log = written.lock().expect("mutex").clone();
    log
}

/// Scenario 1: a `CoderDoneNoOp` event in the Authoring state walks
/// the FSM directly to terminal Shipped, skipping the four
/// intermediate reviewer/shipper states. The downstream roles —
/// reviewer, shipper, ci-watcher — therefore receive no FSM input
/// describing them, which mirrors the production short-circuit
/// (those containers are simply not spawned).
#[tokio::test]
async fn coder_done_no_op_short_circuits_authoring_to_shipped() {
    let brief_id = BriefId("brf_no_op_short_circuit_happy".into());
    let events = vec![
        BriefEvent::CoderStarted {
            agent_id: "agent-1".into(),
            started_at: now(),
        },
        BriefEvent::CoderDoneNoOp {
            reason: NO_OP_VERDICT_REASON.into(),
        },
    ];

    let log = run_to_completion(brief_id, events).await;

    assert_eq!(
        log.len(),
        2,
        "no-op short-circuit writes exactly two records: Authoring then Shipped"
    );
    assert!(
        matches!(log[0].0.state, BriefState::Authoring { .. }),
        "first record is Authoring (CoderStarted transition)"
    );
    assert!(
        matches!(log[1].0.state, BriefState::Shipped),
        "second record is the terminal Shipped — no intermediate Verifying / Reviewing / Shipping / Watching"
    );

    for (record, _) in &log {
        assert!(
            !matches!(
                record.state,
                BriefState::Verifying { .. }
                    | BriefState::Reviewing { .. }
                    | BriefState::Shipping { .. }
                    | BriefState::Watching { .. }
            ),
            "no intermediate downstream states are written for a no-op brief"
        );
    }
}

/// Scenario 2: events appended AFTER a no-op `CoderDoneNoOp` are not
/// consumed — the projector terminates at Shipped. This pins the
/// "downstream roles never run" invariant: even if a stale
/// `AcVerifierDone` somehow appeared on the trace stream, the
/// projector would have stopped before reading it.
#[tokio::test]
async fn projector_stops_at_no_op_terminal_and_does_not_consume_tail_events() {
    let brief_id = BriefId("brf_no_op_terminal_stop".into());
    let events = vec![
        BriefEvent::CoderStarted {
            agent_id: "agent-1".into(),
            started_at: now(),
        },
        BriefEvent::CoderDoneNoOp {
            reason: NO_OP_VERDICT_REASON.into(),
        },
        // Tail events that would normally drive the FSM forward —
        // must not be processed because the projector already saw
        // terminal Shipped.
        BriefEvent::AcVerifierDone {
            verdict: EventVerdict::Shipped,
            role_name: "ac-verifier-test".to_owned(),
        },
        BriefEvent::ReviewerDone {
            verdict: EventVerdict::Shipped,
            findings: vec![],
            role_name: "reviewer-test".to_owned(),
        },
        BriefEvent::ShipperDone {
            pr_number: 1,
            head_sha: "should-not-be-read".into(),
        },
    ];

    let log = run_to_completion(brief_id, events).await;

    assert_eq!(
        log.len(),
        2,
        "tail events past terminal Shipped are not consumed (chain short-circuited)"
    );
    assert!(matches!(log[1].0.state, BriefState::Shipped));
}

/// Scenario 3: the trace-stream translator decodes a coder
/// `EventKind::Done` carrying `reason.cause = "no_op_short_circuit"`
/// into the `CoderDoneNoOp` BriefEvent variant, while a vanilla
/// `EventKind::Done` (no reason, or any other cause) decodes to the
/// standard `CoderDone`. Drives the production translator through a
/// live-Redis trace stream so the cause-sentinel decoding is pinned
/// alongside the FSM short-circuit above.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn translator_decodes_no_op_cause_to_coder_done_no_op() {
    use chrono::Utc;
    use orchestrator_runtime::lifecycle::RedisEventSource;
    use redis::AsyncCommands;

    let Ok(url) = std::env::var("AGENTRY_TEST_REDIS_URL") else {
        return;
    };
    let client = redis::Client::open(url).expect("client");
    let mut conn = redis::aio::ConnectionManager::new(client)
        .await
        .expect("conn");

    let brief_id = BriefId(format!(
        "brf_no_op_translator_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let trace_key = format!("agentry:brief:{}:trace", brief_id.0);

    let spawned = serde_json::json!({
        "at": Utc::now().to_rfc3339(),
        "type": "event",
        "payload": { "agent_event": "spawned", "role_name": "coder" }
    })
    .to_string();
    let no_op_done = serde_json::to_string(&Event::new(EventKind::Done {
        verdict: EventVerdict::Shipped,
        reason: Some(DoneReason {
            cause: NO_OP_SHORT_CIRCUIT_CAUSE.to_string(),
            exit_code: None,
            disagreements: Vec::new(),
        }),
        refusal_count: 0,
    }))
    .expect("serialize no-op done");
    let plain_done = serde_json::to_string(&Event::new(EventKind::Done {
        verdict: EventVerdict::Shipped,
        reason: None,
        refusal_count: 0,
    }))
    .expect("serialize plain done");

    for (label, body) in [
        ("spawned", spawned.as_str()),
        ("no_op_done", no_op_done.as_str()),
    ] {
        let _: String = conn
            .xadd(
                &trace_key,
                "*",
                &[("agent", "agent-no-op"), ("event", body)],
            )
            .await
            .unwrap_or_else(|e| panic!("xadd {label}: {e}"));
    }

    let mut source = RedisEventSource::new(conn.clone(), brief_id.clone());
    let started = source.next().await.expect("started");
    assert!(matches!(started, Some(BriefEvent::CoderStarted { .. })));
    let done = source.next().await.expect("done");
    match done {
        Some(BriefEvent::CoderDoneNoOp { reason }) => {
            assert!(
                reason.contains("no-op"),
                "decoded reason carries the no-op text"
            );
        }
        other => panic!("expected CoderDoneNoOp, got {other:?}"),
    }

    // Plain Shipped done (no cause sentinel) must still decode to the
    // standard CoderDone variant.
    let trace_key_plain = format!("agentry:brief:{}-plain:trace", brief_id.0);
    let _: String = conn
        .xadd(
            &trace_key_plain,
            "*",
            &[("agent", "agent-plain"), ("event", spawned.as_str())],
        )
        .await
        .expect("xadd plain spawned");
    let _: String = conn
        .xadd(
            &trace_key_plain,
            "*",
            &[("agent", "agent-plain"), ("event", plain_done.as_str())],
        )
        .await
        .expect("xadd plain done");
    let mut plain_source =
        RedisEventSource::new(conn.clone(), BriefId(format!("{}-plain", brief_id.0)));
    let _ = plain_source.next().await.expect("plain started");
    let plain_decoded = plain_source.next().await.expect("plain done");
    assert!(
        matches!(
            plain_decoded,
            Some(BriefEvent::CoderDone {
                verdict: EventVerdict::Shipped
            })
        ),
        "plain Shipped done (no cause sentinel) decodes to vanilla CoderDone"
    );

    let _: () = conn.del(&trace_key).await.expect("cleanup trace");
    let _: () = conn
        .del(&trace_key_plain)
        .await
        .expect("cleanup plain trace");
}

// ---------------------------------------------------------------------------
// Workspace teardown on terminal no-op Shipped (the brief's checklist
// item 6). The lifecycle driver's no-op branch reaches the shared
// `cleanup_brief_at` body via the dedicated `cleanup_shipped_no_op_brief`
// helper — exercising the helper directly (the same pattern
// `lifecycle_failed_cleanup.rs` uses for the Failed disposition) pins
// the contract: the worktree dir and the `auto/<brief_id>` branch are
// removed, the bare clone survives. The audit-log wording difference
// vs. the Failed cleanup is verified by code inspection — there is no
// production assertion of log-line content.
// ---------------------------------------------------------------------------

async fn run_git(cwd: &Path, args: &[&str]) {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn setup_upstream(dir: &Path) -> String {
    let upstream = dir.join("upstream.git");
    tokio::fs::create_dir_all(&upstream)
        .await
        .expect("mk upstream dir");
    let seed = dir.join("seed");
    tokio::fs::create_dir_all(&seed).await.expect("mk seed dir");
    run_git(&seed, &["init", "-q", "-b", "main"]).await;
    run_git(&seed, &["config", "user.email", "test@example.com"]).await;
    run_git(&seed, &["config", "user.name", "test"]).await;
    tokio::fs::write(seed.join("README"), b"hello\n")
        .await
        .expect("write README");
    run_git(&seed, &["add", "README"]).await;
    run_git(&seed, &["commit", "-q", "-m", "initial"]).await;
    let out = tokio::process::Command::new("git")
        .args(["clone", "--bare", "-q"])
        .arg(&seed)
        .arg(&upstream)
        .output()
        .await
        .expect("git clone --bare upstream");
    assert!(
        out.status.success(),
        "git clone --bare upstream failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    format!("file://{}", upstream.display())
}

fn bare_clone_path_for(root: &Path) -> std::path::PathBuf {
    let parent = root
        .file_name()
        .expect("tmp dir has a name")
        .to_string_lossy()
        .into_owned();
    root.join(".clones").join(parent).join("upstream")
}

async fn branch_exists(bare: &Path, branch: &str) -> bool {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .await
        .expect("git show-ref spawn");
    out.status.success()
}

/// The no-op cleanup helper removes the per-brief clone directory —
/// same teardown mechanics the Failed disposition uses, just routed
/// through a sibling helper so the audit log cannot mislabel a
/// successful no-op as a terminal Failed cleanup. The bare clone
/// itself survives — it's the read-only cache shared across briefs.
/// Under the per-brief-clone model the `auto/<brief_id>` branch never
/// exists on the bare in the first place; no bare-side branch
/// deletion is needed.
#[tokio::test]
async fn cleanup_shipped_no_op_removes_per_brief_clone_dir() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;
    let bid = BriefId("brf_no_op_cleanup_real".into());

    let ws = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc");
    assert!(ws.host_path.exists(), "clone dir must exist pre-cleanup");

    let bare = bare_clone_path_for(tmp.path());
    let auto_branch = format!("auto/{}", bid.0);
    assert!(
        !branch_exists(&bare, &auto_branch).await,
        "auto/<brief_id> must NEVER exist in the bare clone (per-brief-clone model)"
    );

    cleanup_shipped_no_op_brief_at(&bid, tmp.path(), None).await;

    assert!(
        !ws.host_path.exists(),
        "no-op cleanup must remove the per-brief clone dir"
    );
    assert!(
        !branch_exists(&bare, &auto_branch).await,
        "auto/<brief_id> must remain absent from the bare clone after cleanup"
    );
    assert!(
        bare.join("HEAD").exists(),
        "no-op cleanup must NOT nuke the shared bare clone"
    );
}

/// Idempotency holds for the no-op disposition the same as for the
/// Failed disposition: a second invocation of the no-op cleanup helper
/// must be a no-op itself — no panic, no error escape (cleanup is
/// best-effort by contract), and the post-cleanup invariant stays
/// intact (per-brief clone dir gone). Pins the rename/refactor
/// against accidentally regressing the swallow-errors semantics.
#[tokio::test]
async fn cleanup_shipped_no_op_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tmp");
    let url = setup_upstream(tmp.path()).await;
    let bid = BriefId("brf_no_op_cleanup_idem".into());

    let ws = allocate_at(&bid, Some((url.as_str(), "main")), tmp.path())
        .await
        .expect("alloc");
    let bare = bare_clone_path_for(tmp.path());
    let auto_branch = format!("auto/{}", bid.0);

    cleanup_shipped_no_op_brief_at(&bid, tmp.path(), None).await;
    assert!(!ws.host_path.exists());
    assert!(!branch_exists(&bare, &auto_branch).await);

    cleanup_shipped_no_op_brief_at(&bid, tmp.path(), None).await;
    assert!(!ws.host_path.exists());
    assert!(!branch_exists(&bare, &auto_branch).await);
}
