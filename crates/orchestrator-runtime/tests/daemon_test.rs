//! Public-surface tests for the orchestrator daemon.
//!
//! `daemon.rs`'s prior inline tests exercised private helpers and
//! state-machine internals: `inbound_satisfied`, `downstream_subdag`,
//! `resolve_rework_target`, `mint_permit`, `next_brief_paths`,
//! `load_next_brief`, `collect_chain_paths`, `compose_verdict_parts`,
//! `compose_meta_verdict`, and `handle_brief`. The migration recipe forbids
//! promoting their visibility, so most of those tests are dropped — the
//! behaviours they covered are exercised end-to-end by the existing
//! integration suite: `dispatch_concurrency_cap`,
//! `integration_role_loader_malformed`, `integration_transcript_capture`,
//! `integration_workspace_lifecycle`, and `transcript_parsing`.
//!
//! Exception: `compose_meta_verdict` was promoted to `pub` so the
//! XADD-idempotency gate against `agentry:meta_verdict:emitted:<meta_id>`
//! can be pinned by a live-Redis test (A7v3 reproducer pattern). The DOL
//! terminal-handler is the call site that re-enters this function, so the
//! gate must hold or the operator sees duplicate meta-verdicts on
//! `agentry:verdicts`.

use orchestrator_runtime::daemon::compose_meta_verdict;
use orchestrator_runtime::redis_io::{connect, STREAM_VERDICTS};
use orchestrator_types::{BriefId, FindingOrigin, ReviewFinding, Severity, Verdict, VerdictKind};
use redis::AsyncCommands;

fn test_redis_url() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

fn meta_slug() -> String {
    format!(
        "brf_test_meta_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// A second call to `compose_meta_verdict` for the same `meta_id` must
/// be silenced by the SETNX gate on `agentry:meta_verdict:emitted:<meta_id>`.
/// Without the gate, concurrent DOL terminal callbacks have been observed
/// re-entering the composer (A7v3 reproducer), so XLEN would jump by 2.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn compose_meta_verdict_xadd_gate_silences_second_call() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let meta_id = meta_slug();

    let verdicts_key = format!("agentry:brief:{meta_id}:children_verdicts");
    let pending_key = format!("agentry:brief:{meta_id}:children_pending");
    let verifier_pending_key = format!("agentry:brief:{meta_id}:verifier_pending");
    let verifier_verdict_key = format!("agentry:brief:{meta_id}:verifier_verdict");
    let xadd_emitted_key = format!("agentry:meta_verdict:emitted:{meta_id}");

    // One Shipped child so compose_verdict_parts produces a Shipped meta.
    let child = Verdict::new(BriefId(format!("{meta_id}_child")), VerdictKind::Shipped);
    let child_json = serde_json::to_string(&child).expect("serialize child");
    let _: () = conn
        .rpush(&verdicts_key, child_json)
        .await
        .expect("seed children_verdicts");

    let before: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen before");

    compose_meta_verdict(&mut conn, &meta_id)
        .await
        .expect("first compose");
    let after_first: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen after first");

    compose_meta_verdict(&mut conn, &meta_id)
        .await
        .expect("second compose");
    let after_second: i64 = conn.xlen(STREAM_VERDICTS).await.expect("xlen after second");

    assert_eq!(
        after_first - before,
        1,
        "first compose must XADD exactly one meta verdict"
    );
    assert_eq!(
        after_second - after_first,
        0,
        "second compose for the same meta_id must be silenced by the SETNX gate"
    );

    // Cleanup: the marker key intentionally has a 24h TTL; we drop it
    // explicitly here so per-test runs don't pile up. The other helper
    // keys are deleted by the first compose's cleanup block — DEL on a
    // missing key is a no-op so we can issue them defensively.
    let _: () = conn.del(&xadd_emitted_key).await.expect("cleanup marker");
    let _: () = conn.del(&verdicts_key).await.expect("cleanup verdicts");
    let _: () = conn.del(&pending_key).await.expect("cleanup pending");
    let _: () = conn
        .del(&verifier_pending_key)
        .await
        .expect("cleanup verifier_pending");
    let _: () = conn
        .del(&verifier_verdict_key)
        .await
        .expect("cleanup verifier_verdict");
}

/// #311 fence: when the verifier's verdict is `ReworkNeeded` whose
/// findings all have empty `message`, `requirements`, and `prohibitions`,
/// the daemon must downgrade the published meta verdict to `Shipped`
/// rather than emit `ReworkNeeded` and drain the rework retry budget.
/// Belt + suspenders alongside the reviewer-side fence in
/// `agentry_role_runtime::drop_empty_blocker_findings`.
#[tokio::test]
#[ignore = "requires live Redis (AGENTRY_TEST_REDIS_URL)"]
async fn compose_meta_verdict_downgrades_empty_rework_to_shipped() {
    let Some(url) = test_redis_url() else { return };
    let mut conn = connect(&url).await.expect("connect");
    let meta_id = meta_slug();

    let verdicts_key = format!("agentry:brief:{meta_id}:children_verdicts");
    let pending_key = format!("agentry:brief:{meta_id}:children_pending");
    let verifier_pending_key = format!("agentry:brief:{meta_id}:verifier_pending");
    let verifier_verdict_key = format!("agentry:brief:{meta_id}:verifier_verdict");
    let xadd_emitted_key = format!("agentry:meta_verdict:emitted:{meta_id}");

    let child = Verdict::new(BriefId(format!("{meta_id}_child")), VerdictKind::Shipped);
    let child_json = serde_json::to_string(&child).expect("serialize child");
    let _: () = conn
        .rpush(&verdicts_key, child_json)
        .await
        .expect("seed children_verdicts");

    let empty_finding = ReviewFinding {
        file: None,
        line: None,
        severity: Severity::Blocker,
        origin: FindingOrigin::Model {
            reviewer_agent_id: "agt-test".into(),
        },
        category: "other".into(),
        message: String::new(),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    };
    let verifier = Verdict::new(
        BriefId(format!("{meta_id}_verifier")),
        VerdictKind::ReworkNeeded {
            findings: vec![empty_finding],
        },
    );
    let verifier_json = serde_json::to_string(&verifier).expect("serialize verifier");
    let _: () = conn
        .set(&verifier_verdict_key, verifier_json)
        .await
        .expect("seed verifier verdict");

    compose_meta_verdict(&mut conn, &meta_id)
        .await
        .expect("compose");

    let rev: redis::streams::StreamRangeReply = conn
        .xrevrange_count(STREAM_VERDICTS, "+", "-", 16)
        .await
        .expect("xrevrange");
    let published = rev
        .ids
        .iter()
        .find_map(|entry| {
            let body = entry.map.get("verdict").and_then(|v| match v {
                redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
                redis::Value::SimpleString(s) => Some(s.clone()),
                _ => None,
            })?;
            let v: Verdict = serde_json::from_str(&body).ok()?;
            (v.brief.0 == meta_id).then_some(v)
        })
        .expect("our meta verdict was XADDed");

    assert!(
        matches!(published.kind, VerdictKind::Shipped),
        "all-empty findings must downgrade ReworkNeeded -> Shipped (got {:?})",
        published.kind
    );

    let _: () = conn.del(&xadd_emitted_key).await.expect("cleanup marker");
    let _: () = conn.del(&verdicts_key).await.expect("cleanup verdicts");
    let _: () = conn.del(&pending_key).await.expect("cleanup pending");
    let _: () = conn
        .del(&verifier_pending_key)
        .await
        .expect("cleanup verifier_pending");
    let _: () = conn
        .del(&verifier_verdict_key)
        .await
        .expect("cleanup verifier_verdict");
}
