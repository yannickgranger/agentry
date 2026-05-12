//! Captain `decide` CLI helpers — accept / reject / list briefs parked in
//! `BriefState::AwaitingCaptainDecision`.
//!
//! When the coder runner emits `BriefEvent::CoderDisagreed` (via a Done event
//! whose `DoneReason.cause == "self_review_disagreed"`), the FSM parks the
//! brief in `AwaitingCaptainDecision`. The operator resolves it by pushing
//! one of `BriefEvent::CaptainAccepted` / `BriefEvent::CaptainRejected` onto
//! the brief's trace stream, where the per-brief lifecycle driver consumes
//! it and applies the matching FSM transition.
//!
//! Each helper is one round-trip against Redis: GET `:state`, validate, push
//! the FSM event with the same envelope shape used by `RedisReaperSink::push_event`
//! (`EventKind::Event { payload: <BriefEvent JSON> }` wrapped in a stamped
//! `Event`, XADDed with `agent` set to `captain-cli`).

use crate::{redis_io, Config, Error, Result};
use orchestrator_types::lifecycle::{BriefEvent, BriefState, BriefStateRecord};
use orchestrator_types::{Event, EventKind};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

/// `agent` field used on the trace-stream entry that carries the
/// captain-CLI-pushed decide event. Operators grepping the stream match on
/// this string — distinct from `wall-clock-reaper` (the budget reaper),
/// `operator-cli` (the abort CLI), and `daemon` (the per-brief task).
pub const DECIDE_AGENT_ID: &str = "captain-cli";

/// Accept a parked disagreement: push `BriefEvent::CaptainAccepted` so the
/// FSM transitions through the post-coder phase chain (Verifying → Reviewing
/// → Shipping → Watching) on the work already in the brief workspace.
///
/// # Errors
///
/// Returns `Err(Error::NotFound { kind: "brief", .. })` when no `:state` key
/// exists for `brief_id`. Returns `Err(Error::Config(..))` when the brief is
/// not in `AwaitingCaptainDecision` (the captain CLI mis-targeted a live or
/// terminal brief). Backend / serde failures propagate as `Error::Redis` /
/// `Error::Json`.
pub async fn run_accept(cfg: &Config, brief_id: &str) -> Result<()> {
    let mut conn = redis_io::connect(&cfg.redis.url).await?;
    require_parked(&mut conn, brief_id).await?;
    push_decide_event(&mut conn, brief_id, &BriefEvent::CaptainAccepted).await?;
    println!("{{\"decided\":\"accepted\",\"brief\":\"{brief_id}\"}}");
    Ok(())
}

/// Reject a parked disagreement: push `BriefEvent::CaptainRejected { reason }`
/// so the FSM transitions to `Failed { CaptainRejectedDisagreement { reason } }`.
///
/// # Errors
///
/// Same as [`run_accept`].
pub async fn run_reject(cfg: &Config, brief_id: &str, reason: &str) -> Result<()> {
    let mut conn = redis_io::connect(&cfg.redis.url).await?;
    require_parked(&mut conn, brief_id).await?;
    let event = BriefEvent::CaptainRejected {
        reason: reason.to_string(),
    };
    push_decide_event(&mut conn, brief_id, &event).await?;
    let reason_escaped = json_escape(reason);
    println!(
        "{{\"decided\":\"rejected\",\"brief\":\"{brief_id}\",\"reason\":\"{reason_escaped}\"}}"
    );
    Ok(())
}

/// List every brief currently in `AwaitingCaptainDecision`. Emits one JSON
/// object per line on stdout: `{"brief_id":"…","disagreements":N,"parked_at":"…"}`.
/// Empty stream when nothing is parked.
///
/// # Errors
///
/// Backend / serde failures propagate as `Error::Redis` / `Error::Json`.
pub async fn run_list(cfg: &Config) -> Result<()> {
    let mut conn = redis_io::connect(&cfg.redis.url).await?;
    let parked = list_parked(&mut conn).await?;
    for entry in parked {
        println!(
            "{{\"brief_id\":\"{}\",\"disagreements\":{},\"parked_at\":\"{}\"}}",
            entry.brief_id, entry.disagreements, entry.parked_at,
        );
    }
    Ok(())
}

/// One row produced by [`run_list`]. Internal — `run_list` is the only
/// caller and prints each entry to stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParkedEntry {
    brief_id: String,
    disagreements: usize,
    parked_at: String,
}

async fn require_parked(conn: &mut ConnectionManager, brief_id: &str) -> Result<()> {
    let state_key = format!("agentry:brief:{brief_id}:state");
    let raw: Option<String> = conn.get(&state_key).await?;
    let Some(raw) = raw else {
        return Err(Error::NotFound {
            kind: "brief",
            key: brief_id.to_string(),
        });
    };
    let record: BriefStateRecord = serde_json::from_str(&raw)?;
    if !matches!(record.state, BriefState::AwaitingCaptainDecision { .. }) {
        return Err(Error::Config(format!(
            "brief is not parked (state.kind = {})",
            state_kind_label(&record.state)
        )));
    }
    Ok(())
}

async fn push_decide_event(
    conn: &mut ConnectionManager,
    brief_id: &str,
    event: &BriefEvent,
) -> Result<()> {
    let payload = serde_json::to_value(event)?;
    let trace_event = Event::new(EventKind::Event { payload });
    let body = serde_json::to_string(&trace_event)?;
    let stream = format!("agentry:brief:{brief_id}:trace");
    let _: String = conn
        .xadd(
            &stream,
            "*",
            &[("agent", DECIDE_AGENT_ID), ("event", body.as_str())],
        )
        .await?;
    Ok(())
}

async fn list_parked(conn: &mut ConnectionManager) -> Result<Vec<ParkedEntry>> {
    let mut cursor: u64 = 0;
    let mut keys: Vec<String> = Vec::new();
    loop {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("agentry:brief:*:state")
            .arg("COUNT")
            .arg(100)
            .query_async(conn)
            .await?;
        keys.extend(batch);
        if next == 0 {
            break;
        }
        cursor = next;
    }
    keys.sort();
    let mut out = Vec::new();
    for key in keys {
        let raw: Option<String> = conn.get(&key).await?;
        let Some(raw) = raw else { continue };
        let Ok(record) = serde_json::from_str::<BriefStateRecord>(&raw) else {
            continue;
        };
        if let BriefState::AwaitingCaptainDecision { disagreements, .. } = &record.state {
            out.push(ParkedEntry {
                brief_id: record.brief_id.0.clone(),
                disagreements: disagreements.len(),
                parked_at: record.at.to_rfc3339(),
            });
        }
    }
    Ok(out)
}

fn state_kind_label(state: &BriefState) -> &'static str {
    match state {
        BriefState::Submitted => "submitted",
        BriefState::Authoring { .. } => "authoring",
        BriefState::Verifying { .. } => "verifying",
        BriefState::Reviewing { .. } => "reviewing",
        BriefState::Reworking { .. } => "reworking",
        BriefState::Shipping { .. } => "shipping",
        BriefState::Watching { .. } => "watching",
        BriefState::Extension { .. } => "extension",
        BriefState::AwaitingCaptainDecision { .. } => "awaiting_captain_decision",
        BriefState::Walking { .. } => "walking",
        BriefState::Shipped => "shipped",
        BriefState::Failed { .. } => "failed",
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}
