//! Index-handler coverage for the brief-449c surface: per-row state badges
//! on every active brief and the disagreements section + captain-decide hint
//! on briefs parked awaiting captain decision (post-#495b: encoded as
//! `Walking { run_data: RunData::OperatorDecision { .. }, .. }`).
//!
//! Drives the live `/` route through `tower::ServiceExt::oneshot` against a
//! real `DashboardStore`. Gated on `AGENTRY_TEST_REDIS_URL` so the
//! workspace-wide `cargo test` pass stays green without a Redis dependency.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use orchestrator_dashboard::routes::views;
use orchestrator_dashboard::store::DashboardStore;
use orchestrator_dashboard::AppState;
use orchestrator_types::lifecycle::{
    BriefState, BriefStateRecord, DisagreementSummary, RetryBudget,
};
use orchestrator_types::run_data::RunData;
use orchestrator_types::team::NodeId;
use orchestrator_types::{BriefId, EventVerdict};
use redis::AsyncCommands;
use serde_json::json;
use std::collections::BTreeMap;
use tower::ServiceExt;

fn redis_url_or_skip() -> Option<String> {
    std::env::var("AGENTRY_TEST_REDIS_URL").ok()
}

async fn body_string(body: Body) -> String {
    let bytes = to_bytes(body, 4 * 1024 * 1024).await.expect("body bytes");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

async fn seed_brief(conn: &mut redis::aio::ConnectionManager, brief_id: &str, state: BriefState) {
    let record = BriefStateRecord {
        brief_id: BriefId(brief_id.to_string()),
        state,
        parent_brief_id: None,
        composition_role: None,
        at: chrono::Utc::now(),
    };
    let body = json!({
        "id": brief_id,
        "topology": { "name": "test-topology", "version": 1 },
        "submitted_at": "2026-05-10T00:00:00Z",
    });
    let _: () = conn
        .set(
            format!("agentry:brief:{brief_id}:state"),
            serde_json::to_string(&record).expect("serialize record"),
        )
        .await
        .expect("set state");
    let _: () = conn
        .set(format!("agentry:brief:{brief_id}:body"), body.to_string())
        .await
        .expect("set body");
}

async fn cleanup_brief(conn: &mut redis::aio::ConnectionManager, brief_id: &str) {
    let _: () = conn
        .del(format!("agentry:brief:{brief_id}:state"))
        .await
        .unwrap_or(());
    let _: () = conn
        .del(format!("agentry:brief:{brief_id}:body"))
        .await
        .unwrap_or(());
}

fn fresh_id(tag: &str) -> String {
    format!(
        "brf_test_{tag}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn router_for(state: AppState) -> Router {
    Router::new()
        .route("/", get(views::index))
        .with_state(state)
}

async fn fetch_index(app: Router) -> (StatusCode, String) {
    let res = app
        .oneshot(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router call");
    let status = res.status();
    let body = body_string(res.into_body()).await;
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_includes_state_badges_for_active_briefs() {
    let Some(url) = redis_url_or_skip() else {
        eprintln!("AGENTRY_TEST_REDIS_URL not set — skipping index state-badge test");
        return;
    };
    let store = DashboardStore::new(&url).await.expect("store");
    let client = redis::Client::open(url.as_str()).expect("client");
    let mut conn = redis::aio::ConnectionManager::new(client)
        .await
        .expect("conn");
    let brief_id = fresh_id("authoring");
    // Post-#495b: the coder-running state is `Walking { run_data:
    // RunData::Coder { agent_id }, node_id: <coder role>, .. }`.
    // The dashboard badge surfaces `node_id.0` as the role label.
    seed_brief(
        &mut conn,
        &brief_id,
        BriefState::Walking {
            node_id: NodeId("coder-claude-agentry".to_string()),
            evidence: BTreeMap::<NodeId, EventVerdict>::new(),
            run_data: RunData::Coder {
                agent_id: "agent-1".into(),
            },
            retry: RetryBudget { attempt: 1, max: 3 },
        },
    )
    .await;

    let app = router_for(AppState {
        store,
        webhook_secret: None,
    });
    let (status, body) = fetch_index(app).await;

    cleanup_brief(&mut conn, &brief_id).await;

    assert_eq!(status, StatusCode::OK, "index must return 200");
    assert!(
        body.contains(&brief_id),
        "index should list the seeded brief id"
    );
    assert!(
        body.contains("coder-claude-agentry"),
        "index must render the state badge text matching the current Walking node_id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_renders_disagreements_for_awaiting_captain() {
    let Some(url) = redis_url_or_skip() else {
        eprintln!("AGENTRY_TEST_REDIS_URL not set — skipping awaiting-captain index test");
        return;
    };
    let store = DashboardStore::new(&url).await.expect("store");
    let client = redis::Client::open(url.as_str()).expect("client");
    let mut conn = redis::aio::ConnectionManager::new(client)
        .await
        .expect("conn");
    let brief_id = fresh_id("awaiting");
    // Post-#495b: AwaitingCaptainDecision collapsed into
    // `Walking { run_data: RunData::OperatorDecision { disagreements }, .. }`.
    seed_brief(
        &mut conn,
        &brief_id,
        BriefState::Walking {
            node_id: NodeId("coder-claude-agentry".to_string()),
            evidence: BTreeMap::<NodeId, EventVerdict>::new(),
            run_data: RunData::OperatorDecision {
                disagreements: vec![
                    DisagreementSummary {
                        verb: "REPLACE-X".into(),
                        applied_form: "EDIT".into(),
                        rationale: "literal verb conflicted with surrounding context".into(),
                    },
                    DisagreementSummary {
                        verb: "DELETE-Y".into(),
                        applied_form: "RETAIN".into(),
                        rationale: "removing would break the export surface".into(),
                    },
                ],
            },
            retry: RetryBudget { attempt: 1, max: 3 },
        },
    )
    .await;

    let app = router_for(AppState {
        store,
        webhook_secret: None,
    });
    let (status, body) = fetch_index(app).await;

    cleanup_brief(&mut conn, &brief_id).await;

    assert_eq!(status, StatusCode::OK, "index must return 200");
    assert!(
        body.contains("AWAITING CAPTAIN DECISION"),
        "index must surface the AWAITING CAPTAIN DECISION badge text"
    );
    assert!(
        body.contains("REPLACE-X"),
        "index must surface the first disagreement verb"
    );
    assert!(
        body.contains("DELETE-Y"),
        "index must surface the second disagreement verb"
    );
    assert!(
        body.contains(&format!("captain decide accept {brief_id}")),
        "index must surface the captain-decide accept hint"
    );
    assert!(
        body.contains(&format!("captain decide reject {brief_id}")),
        "index must surface the captain-decide reject hint"
    );
}
