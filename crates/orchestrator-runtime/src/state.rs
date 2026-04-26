//! Agent-state SQLite store. Materialized projection of the trace stream
//! that monitoring layers query. The projector keeps it eventually consistent
//! with Redis; the store itself is pure CRUD plus a read-only `query` escape
//! hatch.

use crate::{Error, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::Mutex;

/// One row of the materialized agent view. Mirrors the `agents` +
/// `cohort_labels` SQLite tables joined on `agent_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub agent_id: String,
    pub brief_id: String,
    pub role_name: String,
    pub project: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
    pub status: String,
    pub verdict: Option<String>,
    pub exit_code: Option<i32>,
    pub cohort_labels: Vec<String>,
}

/// Owning handle to the agent-state store. Wraps a single SQLite connection
/// behind a `tokio::sync::Mutex` so async callers can serialise their writes
/// without dragging the connection into a blocking thread pool.
pub struct State {
    conn: Mutex<Connection>,
}

/// Open (or create) the SQLite agent-state file at `path` and run the
/// idempotent schema migration. Safe to call repeatedly.
pub fn open_or_init(path: &Path) -> Result<State> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agents (
            agent_id TEXT PRIMARY KEY,
            brief_id TEXT NOT NULL,
            role_name TEXT NOT NULL,
            project TEXT,
            started_at TEXT NOT NULL,
            last_event_at TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'running',
            verdict TEXT,
            exit_code INTEGER
        );
        CREATE TABLE IF NOT EXISTS cohort_labels (
            agent_id TEXT NOT NULL,
            label TEXT NOT NULL,
            PRIMARY KEY (agent_id, label)
        );
        CREATE INDEX IF NOT EXISTS idx_agents_brief_id ON agents(brief_id);
        CREATE INDEX IF NOT EXISTS idx_agents_role_name ON agents(role_name);
        CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);",
    )?;
    Ok(State {
        conn: Mutex::new(conn),
    })
}

impl State {
    /// Insert a new agent row or update an existing one (matched by
    /// `agent_id`). `cohort_labels` on `row` are NOT written here — call
    /// [`State::add_cohort_label`] per label.
    pub async fn upsert_agent(&self, row: &AgentRow) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO agents
                (agent_id, brief_id, role_name, project, started_at,
                 last_event_at, status, verdict, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(agent_id) DO UPDATE SET
                brief_id = excluded.brief_id,
                role_name = excluded.role_name,
                project = excluded.project,
                started_at = excluded.started_at,
                last_event_at = excluded.last_event_at,
                status = excluded.status,
                verdict = excluded.verdict,
                exit_code = excluded.exit_code",
            params![
                row.agent_id,
                row.brief_id,
                row.role_name,
                row.project,
                row.started_at.to_rfc3339(),
                row.last_event_at.to_rfc3339(),
                row.status,
                row.verdict,
                row.exit_code,
            ],
        )?;
        Ok(())
    }

    /// Update the `last_event_at` watermark for `agent_id`. No-op if the
    /// agent isn't present (the row may not yet have been upserted).
    pub async fn update_last_event_at(&self, agent_id: &str, ts: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE agents SET last_event_at = ?1 WHERE agent_id = ?2",
            params![ts.to_rfc3339(), agent_id],
        )?;
        Ok(())
    }

    /// Transition `agent_id` to `status='terminated'` and record its
    /// `verdict` + `exit_code`. Idempotent.
    pub async fn mark_terminated(
        &self,
        agent_id: &str,
        verdict: &str,
        exit_code: Option<i32>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE agents
                SET status = 'terminated',
                    verdict = ?1,
                    exit_code = ?2,
                    last_event_at = ?3
                WHERE agent_id = ?4",
            params![verdict, exit_code, Utc::now().to_rfc3339(), agent_id],
        )?;
        Ok(())
    }

    /// Attach a cohort label to `agent_id`. Idempotent — re-adding the same
    /// `(agent_id, label)` pair is a no-op via `INSERT OR IGNORE`.
    pub async fn add_cohort_label(&self, agent_id: &str, label: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO cohort_labels (agent_id, label) VALUES (?1, ?2)",
            params![agent_id, label],
        )?;
        Ok(())
    }

    /// Run a read-only query and return rows as `Vec<HashMap<column, value>>`.
    /// Refuses any SQL whose first whitespace-trimmed token isn't `SELECT` or
    /// `WITH` (case-insensitive). This is the monitoring escape hatch — write
    /// access stays inside the typed methods above.
    pub async fn query(&self, sql: &str) -> Result<Vec<HashMap<String, JsonValue>>> {
        let trimmed = sql.trim_start();
        let first_token = trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if first_token != "SELECT" && first_token != "WITH" {
            return Err(Error::Config(format!(
                "State::query refuses non-readonly SQL (first token: {first_token:?})"
            )));
        }

        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(sql)?;
        let column_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let column_count = column_names.len();
        let mut rows = stmt.query([])?;
        let mut out: Vec<HashMap<String, JsonValue>> = Vec::new();
        while let Some(row) = rows.next()? {
            let mut record: HashMap<String, JsonValue> = HashMap::with_capacity(column_count);
            for (idx, name) in column_names.iter().enumerate() {
                let v = row.get_ref(idx)?;
                record.insert(name.clone(), value_ref_to_json(v));
            }
            out.push(record);
        }
        Ok(out)
    }
}

fn value_ref_to_json(v: rusqlite::types::ValueRef<'_>) -> JsonValue {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(i) => JsonValue::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        ValueRef::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => JsonValue::String(s.to_string()),
            Err(_) => JsonValue::Null,
        },
        ValueRef::Blob(b) => JsonValue::String(format!("<blob:{} bytes>", b.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // Drop and re-open — schema migration must not error on re-run.
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
            .query(
                "SELECT agent_id, status, verdict, exit_code FROM agents WHERE agent_id = 'agt_a'",
            )
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
}
