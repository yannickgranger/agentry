//! Integration tests for `orchestrator agents` CLI handlers.

use chrono::Utc;
use orchestrator_runtime::cli_agents::{list, query};
use orchestrator_runtime::state::{self, AgentRow};

#[tokio::test]
async fn list_returns_running_only_by_default() {
    let dir = tempfile::tempdir().expect("tmp");
    let s = state::open_or_init(&dir.path().join("state.db")).expect("open");
    let now = Utc::now();
    let mk = |id: &str, status: &str| AgentRow {
        agent_id: id.into(),
        brief_id: "brf_x".into(),
        role_name: "coder".into(),
        project: None,
        started_at: now,
        last_event_at: now,
        status: status.into(),
        verdict: None,
        exit_code: None,
        cohort_labels: vec![],
    };
    s.upsert_agent(&mk("agt_a", "running"))
        .await
        .expect("upsert a");
    s.upsert_agent(&mk("agt_b", "terminated"))
        .await
        .expect("upsert b");
    let only_running = list(&s, false).await.expect("list running");
    assert_eq!(only_running.len(), 1);
    let everyone = list(&s, true).await.expect("list all");
    assert_eq!(everyone.len(), 2);
}

#[tokio::test]
async fn query_rejects_writes() {
    let dir = tempfile::tempdir().expect("tmp");
    let s = state::open_or_init(&dir.path().join("state.db")).expect("open");
    assert!(query(&s, "DELETE FROM agents").await.is_err());
}
