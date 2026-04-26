//! Watchdog — long-running task that diagnoses each agent matched by a
//! `Selector` against the agent index, calls xAI Grok-fast for a
//! per-agent stuck/ok judgment, and emits one `EventKind::Status` per
//! agent per tick. Runs alongside the projector inside the daemon.
//!
//! Morphology-agnostic by construction: the unit is the Selector (a SQL
//! string against `State::query`), not the brief or cohort. Cohort
//! labels are query parameters in selectors, never first-class entities
//! here.
//!
//! Cost shape: one Grok-fast HTTPS call per running agent per tick. With
//! 5 running agents and a 60s tick, that's ~300 calls/hour ≈ $0.60/hr.
//!
//! Failure model: any per-agent error (Grok HTTP failure, malformed
//! response, evidence-scan error) is logged and skipped. The watchdog
//! is a best-effort augmentation; it must never crash the daemon or
//! block brief execution. If `XAI_API_KEY` is absent at startup, the
//! daemon does not spawn the watchdog at all — see `daemon.rs`.

use crate::state::State;
use orchestrator_types::{Event, EventKind};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::{json, Value as JsonValue};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_EVIDENCE_CAP: usize = 200;
const DEFAULT_TICK_SECONDS: u64 = 60;
const DEFAULT_GROK_API_URL: &str = "https://api.x.ai/v1/chat/completions";
const DEFAULT_GROK_MODEL: &str = "grok-4-fast";

/// A named read-only SQL query against the agent index. The watchdog
/// tick iterates these; each row in a selector's result becomes one
/// Grok diagnostic call and one Status event.
#[derive(Clone, Debug)]
pub struct Selector {
    pub name: String,
    pub sql: String,
}

/// Watchdog runtime configuration. Constructed once at daemon startup.
#[derive(Clone, Debug)]
pub struct Watchdog {
    pub selectors: Vec<Selector>,
    pub tick_interval: Duration,
    pub grok_api_url: String,
    pub grok_model: String,
    pub grok_api_key: String,
    pub evidence_event_cap: usize,
}

impl Watchdog {
    /// Build a Watchdog with the v1 default selector vec
    /// (a single 'all_running' selector). Caller supplies the API key.
    pub fn new_default(grok_api_key: String) -> Self {
        Self {
            selectors: vec![Selector {
                name: "all_running".into(),
                sql: "SELECT agent_id, brief_id, role_name, project, started_at, last_event_at FROM agents WHERE status = 'running'".into(),
            }],
            tick_interval: Duration::from_secs(
                std::env::var("AGENTRY_WATCHDOG__TICK_SECONDS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_TICK_SECONDS),
            ),
            grok_api_url: std::env::var("AGENTRY_WATCHDOG__GROK_API_URL")
                .unwrap_or_else(|_| DEFAULT_GROK_API_URL.into()),
            grok_model: std::env::var("AGENTRY_WATCHDOG__GROK_MODEL")
                .unwrap_or_else(|_| DEFAULT_GROK_MODEL.into()),
            grok_api_key,
            evidence_event_cap: DEFAULT_EVIDENCE_CAP,
        }
    }
}

/// Run the watchdog forever. Never returns under normal operation;
/// transient errors are logged and retried on the next tick.
pub async fn run(state: Arc<State>, mut conn: ConnectionManager, cfg: Watchdog) -> ! {
    tracing::info!(
        tick_secs = cfg.tick_interval.as_secs(),
        selectors = cfg.selectors.len(),
        model = %cfg.grok_model,
        "watchdog: starting"
    );
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "watchdog: reqwest client init failed; staying dormant");
            futures::future::pending::<()>().await;
            unreachable!()
        }
    };
    let mut ticker = tokio::time::interval(cfg.tick_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(e) = tick(&state, &mut conn, &cfg, &http).await {
            tracing::warn!(error = %e, "watchdog: tick failed (continuing)");
        }
    }
}

async fn tick(
    state: &Arc<State>,
    conn: &mut ConnectionManager,
    cfg: &Watchdog,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    for selector in &cfg.selectors {
        let rows = match state.query(&selector.sql).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(selector = %selector.name, error = %e, "watchdog: selector query failed");
                continue;
            }
        };
        for row in rows {
            if let Err(e) = judge_row(conn, cfg, http, &selector.name, &row).await {
                let aid = row
                    .get("agent_id")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("?");
                tracing::warn!(agent = %aid, error = %e, "watchdog: per-agent judge failed");
            }
        }
    }
    Ok(())
}

async fn judge_row(
    conn: &mut ConnectionManager,
    cfg: &Watchdog,
    http: &reqwest::Client,
    selector_name: &str,
    row: &std::collections::HashMap<String, JsonValue>,
) -> anyhow::Result<()> {
    let agent_id = row
        .get("agent_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow::anyhow!("row missing agent_id"))?;
    let brief_id = row
        .get("brief_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow::anyhow!("row missing brief_id"))?;

    let evidence = scan_evidence(conn, brief_id, agent_id, cfg.evidence_event_cap).await?;
    let evidence_event_ids: Vec<String> = evidence.iter().map(|(id, _)| id.clone()).collect();

    let (ok, stuck, reason) = call_grok(http, cfg, agent_id, &evidence).await?;

    let status = Event::new(EventKind::Status {
        agent_id: agent_id.to_string(),
        ok,
        stuck,
        reason,
        selector_name: selector_name.to_string(),
        evidence_event_ids,
    });
    emit_status(conn, brief_id, agent_id, &status).await?;
    Ok(())
}

/// Read up to `cap` recent entries from the brief's trace stream that
/// match the given agent_id AND are not themselves prior Status events.
/// Returns (entry_id, body) pairs in oldest-to-newest order so the
/// prompt context flows chronologically.
///
/// Filtering Status events is load-bearing: without it, every tick
/// would feed the previous tick's diagnosis back to Grok, producing a
/// self-reinforcing judgment loop and inflating prompt tokens.
async fn scan_evidence(
    conn: &mut ConnectionManager,
    brief_id: &str,
    agent_id: &str,
    cap: usize,
) -> anyhow::Result<Vec<(String, String)>> {
    let stream_key = format!("agentry:brief:{brief_id}:trace");
    let reply: redis::streams::StreamRangeReply = redis::cmd("XREVRANGE")
        .arg(&stream_key)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(cap)
        .query_async(conn)
        .await?;
    let mut out: Vec<(String, String)> = Vec::with_capacity(reply.ids.len());
    for entry in reply.ids {
        let agent_field = entry.map.get("agent").and_then(redis_value_as_str);
        if agent_field.as_deref() != Some(agent_id) {
            continue;
        }
        let body = entry
            .map
            .get("event")
            .and_then(redis_value_as_str)
            .unwrap_or_default();
        if is_status_event_body(&body) {
            continue;
        }
        out.push((entry.id, body));
    }
    out.reverse();
    Ok(out)
}

/// True if the event body is a Status variant. Used by scan_evidence to
/// avoid re-feeding the watchdog's own prior emissions back into Grok.
/// Cheap substring probe rather than a full deserialise — Status events
/// always include `"type":"status"` per the EventKind serde tag.
fn is_status_event_body(body: &str) -> bool {
    body.contains("\"type\":\"status\"") || body.contains("\"type\": \"status\"")
}

fn redis_value_as_str(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
        redis::Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}

async fn call_grok(
    http: &reqwest::Client,
    cfg: &Watchdog,
    agent_id: &str,
    evidence: &[(String, String)],
) -> anyhow::Result<(bool, bool, String)> {
    let system = "You are a watchdog diagnostician for an agent fleet. \
Given an agent's recent trace events (oldest first), judge whether the \
agent is making progress or stuck. Reply with STRICT JSON \
{\"ok\": bool, \"stuck\": bool, \"reason\": \"short string\"}. \
`ok` and `stuck` are mutually exclusive: ok=true means progressing, \
stuck=true means no observable forward motion in the recent tail. \
Reason is one short sentence. Reply with ONLY the JSON, no prose.";
    let user_body = build_user_prompt(agent_id, evidence);

    let req = json!({
        "model": cfg.grok_model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_body},
        ],
        "max_tokens": 256,
    });

    let resp = http
        .post(&cfg.grok_api_url)
        .bearer_auth(&cfg.grok_api_key)
        .json(&req)
        .send()
        .await?
        .error_for_status()?
        .json::<JsonValue>()
        .await?;

    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow::anyhow!("grok response missing /choices/0/message/content"))?;
    parse_judgment(content)
}

fn build_user_prompt(agent_id: &str, evidence: &[(String, String)]) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("Agent id: ");
    out.push_str(agent_id);
    out.push_str("\n\nRecent trace events (oldest first):\n");
    if evidence.is_empty() {
        out.push_str("(no events visible to the watchdog yet)\n");
    } else {
        for (id, body) in evidence {
            out.push_str("- [");
            out.push_str(id);
            out.push_str("] ");
            out.push_str(body);
            out.push('\n');
        }
    }
    out.push_str("\nIs this agent making progress or stuck?");
    out
}

fn parse_judgment(content: &str) -> anyhow::Result<(bool, bool, String)> {
    let trimmed = content.trim();
    let json_str = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_end_matches("```").trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_end_matches("```").trim()
    } else {
        trimmed
    };
    let v: JsonValue = serde_json::from_str(json_str)?;
    let ok = v
        .get("ok")
        .and_then(JsonValue::as_bool)
        .ok_or_else(|| anyhow::anyhow!("missing ok"))?;
    let stuck = v
        .get("stuck")
        .and_then(JsonValue::as_bool)
        .ok_or_else(|| anyhow::anyhow!("missing stuck"))?;
    let reason = v
        .get("reason")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();
    Ok((ok, stuck, reason))
}

async fn emit_status(
    conn: &mut ConnectionManager,
    brief_id: &str,
    agent_id: &str,
    event: &Event,
) -> anyhow::Result<()> {
    let stream_key = format!("agentry:brief:{brief_id}:trace");
    let body = serde_json::to_string(event)?;
    let _: String = conn
        .xadd(
            stream_key.as_str(),
            "*",
            &[("agent", agent_id), ("event", body.as_str())],
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_user_prompt_lists_evidence_chronologically() {
        let ev = vec![
            (
                "100-0".into(),
                "{\"type\":\"event\",\"payload\":{\"msg\":\"first\"}}".into(),
            ),
            (
                "200-0".into(),
                "{\"type\":\"event\",\"payload\":{\"msg\":\"second\"}}".into(),
            ),
        ];
        let p = build_user_prompt("agt_x", &ev);
        assert!(p.contains("agt_x"));
        assert!(p.contains("[100-0]"));
        assert!(p.contains("[200-0]"));
        assert!(p.find("[100-0]").expect("100-0") < p.find("[200-0]").expect("200-0"));
    }

    #[test]
    fn build_user_prompt_handles_empty_evidence() {
        let p = build_user_prompt("agt_y", &[]);
        assert!(p.contains("no events visible"));
        assert!(p.contains("agt_y"));
    }

    #[test]
    fn parse_judgment_strict_json() {
        let (ok, stuck, reason) =
            parse_judgment("{\"ok\": true, \"stuck\": false, \"reason\": \"progressing\"}")
                .expect("parse");
        assert!(ok);
        assert!(!stuck);
        assert_eq!(reason, "progressing");
    }

    #[test]
    fn parse_judgment_tolerates_fenced_block() {
        let s = "```json\n{\"ok\": false, \"stuck\": true, \"reason\": \"no movement\"}\n```";
        let (ok, stuck, reason) = parse_judgment(s).expect("parse fenced");
        assert!(!ok);
        assert!(stuck);
        assert_eq!(reason, "no movement");
    }

    #[test]
    fn parse_judgment_rejects_missing_fields() {
        assert!(parse_judgment("{\"ok\": true}").is_err());
        assert!(parse_judgment("{\"stuck\": false}").is_err());
        assert!(parse_judgment("not json").is_err());
    }

    #[test]
    fn is_status_event_body_detects_typed_variant() {
        assert!(is_status_event_body(
            "{\"at\":\"...\",\"type\":\"status\",\"agent_id\":\"agt_x\"}"
        ));
        assert!(is_status_event_body("{\"type\": \"status\"}"));
        assert!(!is_status_event_body("{\"type\":\"event\",\"payload\":{}}"));
        assert!(!is_status_event_body(
            "{\"type\":\"done\",\"verdict\":\"shipped\"}"
        ));
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
        use crate::state;
        use chrono::Utc;
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("state.db");
        let s = state::open_or_init(&path).expect("open");
        let now = Utc::now();
        let mk = |id: &str, status: &str| crate::state::AgentRow {
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
}
