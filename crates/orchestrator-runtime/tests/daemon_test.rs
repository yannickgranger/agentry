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
use orchestrator_types::{BriefId, Verdict, VerdictKind};
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
