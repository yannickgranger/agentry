//! Integration tests for the agent-state SQLite store.

use chrono::Utc;
use orchestrator_runtime::state::{open_or_init, AgentRow};
use serde_json::Value as JsonValue;
use tempfile::tempdir;

fn sample_row(id: &str) -> AgentRow {
    AgentRow {
        agent_id: id.into(),
        brief_id: "brf_test".into(),
        role_name: "coder".into(),
        project: Some("agentry".into()),
        started_at: Utc::now(),
        last_event_at: Utc::now(),
        status: "running".into(),
        verdict: None,
        exit_code: None,
        cohort_labels: vec![],
    }
}

#[tokio::test]
async fn open_or_init_is_idempotent() {
    let dir = tempdir().expect("tmp");
    let path = dir.path().join("state.db");
    let _s1 = open_or_init(&path).expect("open 1");
    let _s2 = open_or_init(&path).expect("open 2");
}

#[tokio::test]
async fn upsert_then_mark_terminated() {
    let dir = tempdir().expect("tmp");
    let path = dir.path().join("state.db");
    let s = open_or_init(&path).expect("open");
    s.upsert_agent(&sample_row("agt_a")).await.expect("upsert");
    s.add_cohort_label("agt_a", "fleet:self-host")
        .await
        .expect("label");
    s.mark_terminated("agt_a", "shipped", Some(0))
        .await
        .expect("terminate");

    let rows = s
        .query("SELECT agent_id, status, verdict, exit_code FROM agents WHERE agent_id = 'agt_a'")
        .await
        .expect("query");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["agent_id"], JsonValue::from("agt_a"));
    assert_eq!(row["status"], JsonValue::from("terminated"));
    assert_eq!(row["verdict"], JsonValue::from("shipped"));
    assert_eq!(row["exit_code"], JsonValue::from(0i64));
}

#[tokio::test]
async fn query_refuses_writes() {
    let dir = tempdir().expect("tmp");
    let path = dir.path().join("state.db");
    let s = open_or_init(&path).expect("open");
    let err = s
        .query("DELETE FROM agents")
        .await
        .expect_err("must reject");
    let msg = format!("{err}");
    assert!(msg.contains("non-readonly"), "msg: {msg}");
}

#[tokio::test]
async fn query_accepts_select_and_with() {
    let dir = tempdir().expect("tmp");
    let path = dir.path().join("state.db");
    let s = open_or_init(&path).expect("open");
    s.upsert_agent(&sample_row("agt_b")).await.expect("upsert");
    let rows = s
        .query("WITH x AS (SELECT agent_id FROM agents) SELECT * FROM x")
        .await
        .expect("with-select runs");
    assert_eq!(rows.len(), 1);
    let rows = s
        .query("  SELECT agent_id FROM agents")
        .await
        .expect("leading whitespace ok");
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn add_cohort_label_is_idempotent() {
    let dir = tempdir().expect("tmp");
    let path = dir.path().join("state.db");
    let s = open_or_init(&path).expect("open");
    s.upsert_agent(&sample_row("agt_c")).await.expect("upsert");
    s.add_cohort_label("agt_c", "phase:plan").await.expect("1");
    s.add_cohort_label("agt_c", "phase:plan").await.expect("2");
    let rows = s
        .query("SELECT label FROM cohort_labels WHERE agent_id = 'agt_c'")
        .await
        .expect("query");
    assert_eq!(rows.len(), 1);
}
