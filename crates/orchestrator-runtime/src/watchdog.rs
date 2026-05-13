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
use orchestrator_infra::redis_io::redis_value_as_str;
use orchestrator_types::{Event, EventKind};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_EVIDENCE_CAP: usize = 200;
const DEFAULT_TICK_SECONDS: u64 = 60;
const DEFAULT_GROK_API_URL: &str = "https://api.x.ai/v1/chat/completions";
const DEFAULT_GROK_MODEL: &str = "grok-4-fast";
/// Default consecutive-stuck count before the watchdog kills the agent
/// container and lets the spawner emit a Failed verdict via its
/// exit-without-Done fall-through.
const DEFAULT_STUCK_THRESHOLD: u32 = 3;
const DEFAULT_DISTINCT_PAYLOAD_THRESHOLD: usize = 2;

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
    /// Number of consecutive `stuck=true` Status verdicts on the same
    /// agent before the watchdog kills its container. Env-overridable
    /// via `AGENTRY_WATCHDOG__STUCK_THRESHOLD`.
    pub stuck_threshold: u32,
    /// Minimum number of distinct event payloads in the recent
    /// evidence tail required before a `stuck=true` verdict is allowed
    /// to escalate to a container kill. Defends against false positives
    /// on legitimate long-poll-loop agents (e.g. ci-watcher) whose tail
    /// is the same payload repeating but whose work is healthy.
    /// Env-overridable via `AGENTRY_WATCHDOG__DISTINCT_PAYLOAD_THRESHOLD`.
    pub distinct_payload_threshold: usize,
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
            stuck_threshold: std::env::var("AGENTRY_WATCHDOG__STUCK_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_STUCK_THRESHOLD),
            distinct_payload_threshold: std::env::var("AGENTRY_WATCHDOG__DISTINCT_PAYLOAD_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DISTINCT_PAYLOAD_THRESHOLD),
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
    let mut stuck_counts: HashMap<String, u32> = HashMap::new();
    loop {
        ticker.tick().await;
        if let Err(e) = tick(&state, &mut conn, &cfg, &http, &mut stuck_counts).await {
            tracing::warn!(error = %e, "watchdog: tick failed (continuing)");
        }
    }
}

async fn tick(
    state: &Arc<State>,
    conn: &mut ConnectionManager,
    cfg: &Watchdog,
    http: &reqwest::Client,
    stuck_counts: &mut HashMap<String, u32>,
) -> anyhow::Result<()> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for selector in &cfg.selectors {
        let rows = match state.query(&selector.sql).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(selector = %selector.name, error = %e, "watchdog: selector query failed");
                continue;
            }
        };
        for row in rows {
            if let Some(aid) = row.get("agent_id").and_then(JsonValue::as_str) {
                seen.insert(aid.to_string());
            }
            if let Err(e) = judge_row(conn, cfg, http, &selector.name, &row, stuck_counts).await {
                let aid = row
                    .get("agent_id")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("?");
                tracing::warn!(agent = %aid, error = %e, "watchdog: per-agent judge failed");
            }
        }
    }
    // Prune count map: any entry whose agent did not appear in this
    // tick's selector results is a stale entry (agent terminated
    // naturally, or was killed by us in a prior tick). Drop it so the
    // map stays bounded by the number of currently-running agents.
    stuck_counts.retain(|aid, _| seen.contains(aid));
    Ok(())
}

async fn judge_row(
    conn: &mut ConnectionManager,
    cfg: &Watchdog,
    http: &reqwest::Client,
    selector_name: &str,
    row: &std::collections::HashMap<String, JsonValue>,
    stuck_counts: &mut HashMap<String, u32>,
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

    // Update consecutive-stuck counter for this agent.
    let new_count = update_stuck_count(stuck_counts, agent_id, stuck);
    let distinct = distinct_payload_count(&evidence);
    if stuck && new_count >= cfg.stuck_threshold && distinct >= cfg.distinct_payload_threshold {
        // Threshold reached — kill the container and emit an audit event.
        // The Status event with the triggering verdict is still emitted
        // below, so consumers see the final stuck judgment that fired
        // the kill. After emit, the spawner's stdout reader sees EOF
        // (kill) and `compute_verdict` returns Failed via its
        // exit-without-Done fall-through; daemon's team-failure path
        // then preserves the workspace for audit.
        emit_kill_annotation(conn, brief_id, agent_id, new_count, &reason).await?;
        if let Err(e) = kill_container(agent_id).await {
            tracing::warn!(agent = %agent_id, error = %e, "watchdog: kill_container failed");
        }
        // Drop the entry so a (theoretical) re-spawn of the same agent
        // id starts fresh.
        stuck_counts.remove(agent_id);
    } else if stuck && new_count >= cfg.stuck_threshold {
        tracing::debug!(
            agent = %agent_id,
            consecutive_stuck = new_count,
            distinct_payload_count = distinct,
            distinct_payload_threshold = cfg.distinct_payload_threshold,
            "watchdog: stuck threshold reached but tail is uniform (poll-loop signature) — not escalating"
        );
    }

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

/// Update the consecutive-stuck counter for `agent_id` based on the
/// current tick's `stuck` judgment. Returns the new count. ok=true
/// resets to 0 (and removes the entry); stuck=true increments.
fn update_stuck_count(counts: &mut HashMap<String, u32>, agent_id: &str, stuck: bool) -> u32 {
    if stuck {
        let entry = counts.entry(agent_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    } else {
        counts.remove(agent_id);
        0
    }
}

/// Count distinct event payload bodies in the evidence tail. Used
/// to distinguish a healthy poll-loop tail (same payload repeating,
/// distinct count low) from a genuinely stuck tail (variety of
/// events petering out, distinct count moderate-to-high). Trades
/// off allocation cost (one HashSet of borrowed &str) against
/// avoiding a false-positive kill — acceptable on a 60s tick.
fn distinct_payload_count(evidence: &[(String, String)]) -> usize {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (_, body) in evidence {
        seen.insert(body.as_str());
    }
    seen.len()
}

/// XADD an audit-trail annotation to the agent's brief trace stream
/// recording that the watchdog killed the container after N
/// consecutive stuck verdicts. Mirrors the spawner's spawn-event
/// emission shape so the projector advances the watermark.
async fn emit_kill_annotation(
    conn: &mut ConnectionManager,
    brief_id: &str,
    agent_id: &str,
    consecutive_stuck: u32,
    reason: &str,
) -> anyhow::Result<()> {
    let stream_key = format!("agentry:brief:{brief_id}:trace");
    let event = Event::new(EventKind::Event {
        payload: serde_json::json!({
            "agent_event": "watchdog_kill",
            "consecutive_stuck": consecutive_stuck,
            "reason": reason,
        }),
    });
    let body = serde_json::to_string(&event)?;
    let _: String = conn
        .xadd(
            stream_key.as_str(),
            "*",
            &[("agent", agent_id), ("event", body.as_str())],
        )
        .await?;
    Ok(())
}

/// Best-effort `podman kill` against the container running this agent.
/// Container name is `agentry-<agent_id>` per the spawner's naming
/// scheme. A non-zero exit is logged at warn (the container may have
/// already exited on its own between the watchdog's last observation
/// and this kill); the watchdog does not treat it as a failure.
async fn kill_container(agent_id: &str) -> anyhow::Result<()> {
    let name = format!("agentry-{agent_id}");
    let out = tokio::process::Command::new("podman")
        .arg("kill")
        .arg("--ignore")
        .arg(&name)
        .output()
        .await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let exit = out.status.code().unwrap_or(-1);
        tracing::warn!(
            "watchdog: podman kill --ignore returned non-zero (continuing) container={name} exit={exit} stderr={stderr} stdout={stdout}"
        );
    }
    Ok(())
}
