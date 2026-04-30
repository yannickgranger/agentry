//! Concrete trace-metric producer.
//!
//! Reads three existing data sources for a brief and folds them into a
//! single `TraceMetric` value:
//!   * `agentry:brief:{brief_id}:trace` — Redis stream of agent events.
//!   * `/transcripts/{brief_id}.jsonl`  — claude transcript on disk.
//!   * `agentry:audit:tool-calls:{brief_id}` — tool-call audit log.
//!
//! v1 is best-effort: if a source is unreachable, the corresponding
//! fields are emitted as zero rather than failing the whole aggregate.

use std::fs;

use serde::{Deserialize, Serialize};

const TRANSCRIPTS_DIR: &str = "/transcripts";

/// One brief's worth of folded trace evidence. Field set is fixed by
/// `specs/concepts/trace_metric.md#TraceMetric`; do not extend without a
/// corresponding spec change.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TraceMetric {
    #[serde(default)]
    pub brief_id: String,
    #[serde(default)]
    pub compile_cycles: u32,
    #[serde(default)]
    pub reads_before_first_edit: u32,
    #[serde(default)]
    pub refusal_count: u32,
    #[serde(default)]
    pub wall_seconds: u64,
    #[serde(default)]
    pub lines_changed: u32,
    #[serde(default)]
    pub verb_citation_density: f32,
}

/// Fold the three data sources for `brief_id` into a `TraceMetric`.
///
/// Best-effort: an unreachable source contributes zero to its fields.
/// Returns `Err` only when the call cannot produce even a partial
/// result (which today is never — every source is guarded individually).
pub fn aggregate(brief_id: &str, redis: &mut redis::Connection) -> anyhow::Result<TraceMetric> {
    let mut metric = TraceMetric {
        brief_id: brief_id.to_string(),
        ..TraceMetric::default()
    };

    if let Ok(events) = read_trace_stream(brief_id, redis) {
        metric.compile_cycles = count_compile_cycles(&events);
        metric.reads_before_first_edit = count_reads_before_first_edit(&events);
        metric.refusal_count = count_refusals(&events);
    }

    let transcript = format!("{TRANSCRIPTS_DIR}/{brief_id}.jsonl");
    if let Ok(body) = fs::read_to_string(&transcript) {
        metric.wall_seconds = wall_seconds_from_transcript(&body);
        // lines_changed left at 0 for v1 — transcript line-count
        // deltas require parsing tool-result payloads in a way that
        // is not yet stable across claude versions.
    }

    // verb_citation_density left at 0.0 for v1 — the brief payload's
    // verb list is not yet wired into the audit log shape used by all
    // roles. Reading the audit log is still attempted so a future
    // implementation can evolve the field without changing the
    // aggregate signature.
    let _ = read_audit_log(brief_id, redis);

    Ok(metric)
}

/// XRANGE the brief's trace stream and pull out each entry's `event`
/// field as a parsed JSON value. Empty streams return an empty Vec.
fn read_trace_stream(
    brief_id: &str,
    conn: &mut redis::Connection,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let stream_key = format!("agentry:brief:{brief_id}:trace");
    let reply: redis::streams::StreamRangeReply = redis::cmd("XRANGE")
        .arg(&stream_key)
        .arg("-")
        .arg("+")
        .query(conn)?;
    let mut out = Vec::with_capacity(reply.ids.len());
    for entry in reply.ids {
        if let Some(body) = entry.map.get("event").and_then(redis_value_as_str) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                out.push(v);
            }
        }
    }
    Ok(out)
}

/// LRANGE the brief's tool-call audit log. Stored as a Redis list of
/// JSON-encoded strings, one per tool call, in append order.
fn read_audit_log(
    brief_id: &str,
    conn: &mut redis::Connection,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let key = format!("agentry:audit:tool-calls:{brief_id}");
    let entries: Vec<String> = redis::cmd("LRANGE").arg(&key).arg(0).arg(-1).query(conn)?;
    let mut out = Vec::with_capacity(entries.len());
    for s in entries {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            out.push(v);
        }
    }
    Ok(out)
}

fn redis_value_as_str(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
        redis::Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}

/// True iff the event is a Bash tool_call whose `command` arg invokes
/// the rust toolchain in a way that triggers a compile cycle (check,
/// build, test, clippy).
fn is_compile_cycle(event: &serde_json::Value) -> bool {
    if event.get("type").and_then(|v| v.as_str()) != Some("tool_call") {
        return false;
    }
    let call = event.get("call");
    let tool = call.and_then(|c| c.get("tool")).and_then(|v| v.as_str());
    if tool != Some("Bash") {
        return false;
    }
    let cmd = call
        .and_then(|c| c.get("args"))
        .and_then(|a| a.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    cmd.contains("cargo check")
        || cmd.contains("cargo build")
        || cmd.contains("cargo test")
        || cmd.contains("cargo clippy")
}

fn tool_name(event: &serde_json::Value) -> Option<&str> {
    if event.get("type").and_then(|v| v.as_str()) != Some("tool_call") {
        return None;
    }
    event
        .get("call")
        .and_then(|c| c.get("tool"))
        .and_then(|v| v.as_str())
}

fn is_refusal(event: &serde_json::Value) -> bool {
    if event.get("type").and_then(|v| v.as_str()) == Some("tool_refused") {
        return true;
    }
    // Tolerate freeform events whose payload carries an explicit
    // `refused` flag — older roles emitted refusals this way.
    event
        .get("payload")
        .and_then(|p| p.get("refused"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn count_compile_cycles(events: &[serde_json::Value]) -> u32 {
    u32::try_from(events.iter().filter(|e| is_compile_cycle(e)).count()).unwrap_or(u32::MAX)
}

fn count_reads_before_first_edit(events: &[serde_json::Value]) -> u32 {
    let mut reads: u32 = 0;
    for event in events {
        match tool_name(event) {
            Some("Edit" | "Write" | "NotebookEdit") => return reads,
            Some("Read") => reads = reads.saturating_add(1),
            _ => {}
        }
    }
    reads
}

fn count_refusals(events: &[serde_json::Value]) -> u32 {
    u32::try_from(events.iter().filter(|e| is_refusal(e)).count()).unwrap_or(u32::MAX)
}

/// First→last `at` timestamp delta across the transcript's JSONL lines,
/// in whole seconds. Returns 0 for an empty transcript or one whose
/// timestamps don't parse as RFC3339.
fn wall_seconds_from_transcript(body: &str) -> u64 {
    let mut first: Option<i64> = None;
    let mut last: Option<i64> = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(ts) = parse_ts(&v) else { continue };
        first.get_or_insert(ts);
        last = Some(ts);
    }
    match (first, last) {
        (Some(a), Some(b)) if b >= a => u64::try_from(b - a).unwrap_or(0),
        _ => 0,
    }
}

fn parse_ts(v: &serde_json::Value) -> Option<i64> {
    let s = v
        .get("at")
        .or_else(|| v.get("timestamp"))
        .and_then(|t| t.as_str())?;
    parse_rfc3339_secs(s)
}

/// Tiny RFC3339-to-epoch-seconds parser. Whole seconds only — chrono
/// would be overkill for a one-shot transcript scan, and trace-query's
/// dep set is constrained by spec.
fn parse_rfc3339_secs(s: &str) -> Option<i64> {
    // Accept e.g. 2026-04-30T12:34:56Z or 2026-04-30T12:34:56.123+00:00.
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    Some(
        days_from_civil(year, month, day) * 86400
            + i64::from(hour) * 3600
            + i64::from(minute) * 60
            + i64::from(second),
    )
}

/// Howard Hinnant's days-from-civil. Returns days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = u64::try_from(y - era * 400).unwrap_or(0);
    let m = u64::from(m);
    let d = u64::from(d);
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + i64::try_from(doe).unwrap_or(0) - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_call_bash(cmd: &str) -> serde_json::Value {
        json!({
            "at": "2026-04-30T00:00:00Z",
            "type": "tool_call",
            "call": { "tool": "Bash", "args": { "command": cmd } },
        })
    }

    fn tool_call(tool: &str) -> serde_json::Value {
        json!({
            "at": "2026-04-30T00:00:00Z",
            "type": "tool_call",
            "call": { "tool": tool, "args": {} },
        })
    }

    #[test]
    fn compile_cycles_counts_cargo_invocations() {
        let events = vec![
            tool_call_bash("cargo check --workspace"),
            tool_call_bash("ls -la"),
            tool_call_bash("cargo test -p foo"),
            tool_call_bash("cargo clippy --workspace -- -D warnings"),
            tool_call_bash("echo hi"),
            tool_call_bash("cargo build"),
        ];
        assert_eq!(count_compile_cycles(&events), 4);
    }

    #[test]
    fn reads_before_first_edit_stops_at_edit() {
        let events = vec![
            tool_call("Read"),
            tool_call("Read"),
            tool_call("Grep"),
            tool_call("Read"),
            tool_call("Edit"),
            tool_call("Read"),
        ];
        assert_eq!(count_reads_before_first_edit(&events), 3);
    }

    #[test]
    fn reads_before_first_edit_counts_all_when_no_edit() {
        let events = vec![tool_call("Read"), tool_call("Grep"), tool_call("Read")];
        assert_eq!(count_reads_before_first_edit(&events), 2);
    }

    #[test]
    fn refusals_count_explicit_and_payload_flagged() {
        let events = vec![
            json!({ "type": "tool_refused" }),
            json!({ "type": "event", "payload": { "refused": true } }),
            json!({ "type": "event", "payload": { "msg": "hi" } }),
            json!({ "type": "tool_refused" }),
        ];
        assert_eq!(count_refusals(&events), 3);
    }

    #[test]
    fn wall_seconds_from_first_and_last_timestamp() {
        let body = "\
{\"at\":\"2026-04-30T00:00:00Z\",\"type\":\"event\"}
{\"at\":\"2026-04-30T00:01:30Z\",\"type\":\"event\"}
{\"at\":\"2026-04-30T00:05:00Z\",\"type\":\"event\"}
";
        assert_eq!(wall_seconds_from_transcript(body), 300);
    }

    #[test]
    fn wall_seconds_zero_when_no_timestamps() {
        assert_eq!(wall_seconds_from_transcript(""), 0);
        assert_eq!(wall_seconds_from_transcript("not json\n"), 0);
    }

    #[test]
    fn trace_metric_default_round_trips() {
        let m = TraceMetric::default();
        let s = serde_json::to_string(&m).expect("ser");
        let back: TraceMetric = serde_json::from_str(&s).expect("de");
        assert_eq!(m, back);
    }

    #[test]
    fn trace_metric_deserialises_with_missing_fields() {
        // backward-compat: serde(default) on every field means an
        // upstream consumer can drop fields without breaking parsing.
        let m: TraceMetric = serde_json::from_str("{\"brief_id\":\"x\"}").expect("de");
        assert_eq!(m.brief_id, "x");
        assert_eq!(m.compile_cycles, 0);
    }
}
