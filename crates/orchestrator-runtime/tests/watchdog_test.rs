//! Public-surface tests for the watchdog.
//!
//! `watchdog.rs`'s prior inline tests exercised private helpers
//! (`build_user_prompt`, `parse_judgment`, `is_status_event_body`,
//! `update_stuck_count`, `distinct_payload_count`) directly. The migration
//! recipe forbids promoting their visibility, so those tests are dropped —
//! the watchdog's stuck-detection and Grok-call behaviour is exercised
//! end-to-end by the daemon integration suite.
//!
//! What survives here are the construction-and-selector tests that only
//! touch the public `Watchdog::new_default` constructor and the public
//! `state::open_or_init` / `State::query` surface used by the daemon at
//! runtime to drive the watchdog tick.

use orchestrator_runtime::state;
use orchestrator_runtime::watchdog::Watchdog;
use serde_json::Value as JsonValue;

#[test]
fn watchdog_default_distinct_payload_threshold_is_two() {
    let w = Watchdog::new_default("k".into());
    assert_eq!(w.distinct_payload_threshold, 2);
}

#[test]
fn watchdog_default_threshold_is_three() {
    let w = Watchdog::new_default("k".into());
    assert_eq!(w.stuck_threshold, 3);
}

#[test]
fn watchdog_default_has_one_selector_named_all_running() {
    let w = Watchdog::new_default("test-key".into());
    assert_eq!(w.selectors.len(), 1);
    assert_eq!(w.selectors[0].name, "all_running");
    assert!(w.selectors[0].sql.to_lowercase().contains("select"));
    assert!(w.selectors[0].sql.to_lowercase().contains("from agents"));
}

#[tokio::test]
async fn selector_sql_runs_against_state_and_returns_running_only() {
    use chrono::Utc;
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("state.db");
    let s = state::open_or_init(&path).expect("open");
    let now = Utc::now();
    let mk = |id: &str, status: &str| state::AgentRow {
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
    s.upsert_agent(&mk("agt_running", "running"))
        .await
        .expect("a");
    s.upsert_agent(&mk("agt_done", "terminated"))
        .await
        .expect("b");
    let w = Watchdog::new_default("test".into());
    let rows = s.query(&w.selectors[0].sql).await.expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["agent_id"], JsonValue::from("agt_running"));
}
