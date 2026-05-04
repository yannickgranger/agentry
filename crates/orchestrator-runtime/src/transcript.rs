//! Pure parsing of `claude -p --output-format stream-json --verbose` transcripts.
//!
//! No I/O — callers feed us a `&str` (typically the contents of a
//! `/var/lib/agentry/transcripts/<brief>.<role>.jsonl` file) and we return
//! parsed events plus optional summarization. Mid-stream truncation (a
//! partial trailing line, e.g. when `claude -p` is killed by `timeout`) is
//! tolerated: any line that doesn't parse as JSON is dropped silently
//! provided it's the last line, otherwise it's logged at WARN.
//!
//! The schema mirrors what `BASH_PRELUDE::stream_claude` writes (see
//! `seed.rs`): each line is one stream-json envelope with a `type` discriminant.
//!
//! `claude -p` stream-json envelopes do NOT carry per-event timestamps,
//! so the parser cannot derive `started_at`/`completed_at` from the data.
//! To keep the module pure (no `Utc::now()` reads inside), the caller
//! supplies a [`TranscriptTimes`] pair: `started_at` (e.g. file ctime or
//! the brief's spawn time) and `last_event_at` (e.g. file mtime). The
//! parser stamps the first parsed event with `started_at` and every
//! subsequent event with `last_event_at`, so `summarize` reports a
//! meaningful `wall_clock_secs` (= last - first) and
//! `extract_last_tool_call(events, now)` reports a non-zero
//! `duration_so_far_secs` for an in-flight tool call.
//!
//! See the module test
//! `unfinished_tool_call_has_nonzero_duration_when_now_after_last_event`
//! and the parser tests in `tests/transcript_parsing.rs` — together they
//! lock the time-bearing-fields contract down.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Caller-supplied timing source. Both fields are wall-clock UTC.
///
/// `started_at` is when the transcript file (or the brief's claude run)
/// began — typically the file's ctime/birthtime, falling back to mtime.
/// `last_event_at` is the file's mtime, which bounds the most recent
/// activity observed on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptTimes {
    pub started_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
}

/// One parsed stream-json event from a `claude -p` transcript.
///
/// The variants cover the four `type`s `stream_claude` produces; anything
/// else is wrapped in `Other` so unknown event kinds don't lose data.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEvent {
    SystemInit {
        session_id: Option<String>,
        model: Option<String>,
        at: DateTime<Utc>,
    },
    Assistant {
        tool_uses: Vec<ToolUse>,
        text: Option<String>,
        tokens_in: u64,
        tokens_out: u64,
        at: DateTime<Utc>,
    },
    User {
        tool_results: Vec<ToolResult>,
        at: DateTime<Utc>,
    },
    Result {
        success: bool,
        text: Option<String>,
        duration_ms: Option<u64>,
        total_cost_usd: Option<f64>,
        at: DateTime<Utc>,
    },
    Other {
        raw: Value,
        at: DateTime<Utc>,
    },
}

impl TranscriptEvent {
    #[must_use]
    pub fn at(&self) -> DateTime<Utc> {
        match self {
            Self::SystemInit { at, .. }
            | Self::Assistant { at, .. }
            | Self::User { at, .. }
            | Self::Result { at, .. }
            | Self::Other { at, .. } => *at,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub output: Value,
    pub completed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastToolCall {
    pub tool: String,
    pub input: Value,
    pub started_at: DateTime<Utc>,
    pub duration_so_far_secs: u64,
    pub completed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptSummary {
    pub tool_histogram: BTreeMap<String, u64>,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub wall_clock_secs: u64,
    pub event_count: u64,
    pub first_event_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
}

/// Parse the transcript JSONL into a Vec of typed events using
/// caller-supplied timing.
///
/// The first parsed event is stamped with `times.started_at`; every
/// subsequent event with `times.last_event_at`. Tolerates a partial
/// trailing line (the `timeout`-kill case): if the final line fails to
/// parse as JSON it's dropped silently. Earlier malformed lines are
/// dropped with a `tracing::warn`.
#[must_use]
pub fn parse_jsonl_lines(s: &str, times: TranscriptTimes) -> Vec<TranscriptEvent> {
    let lines: Vec<&str> = s.lines().collect();
    let total = lines.len();
    let mut out: Vec<TranscriptEvent> = Vec::with_capacity(total);
    for (idx, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => {
                if idx + 1 == total {
                    continue;
                }
                tracing::warn!(line = %line, error = %err, "transcript: skipped malformed line");
                continue;
            }
        };
        let at = if out.is_empty() {
            times.started_at
        } else {
            times.last_event_at
        };
        out.push(parse_value(value, at));
    }
    out
}

fn parse_value(value: Value, at: DateTime<Utc>) -> TranscriptEvent {
    let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "system" => {
            let session_id = value
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let model = value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            TranscriptEvent::SystemInit {
                session_id,
                model,
                at,
            }
        }
        "assistant" => parse_assistant(&value, at),
        "user" => parse_user(&value, at),
        "result" => {
            let success = value.get("subtype").and_then(Value::as_str) == Some("success")
                || !value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let text = value
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_string);
            let duration_ms = value.get("duration_ms").and_then(Value::as_u64);
            let total_cost_usd = value.get("total_cost_usd").and_then(Value::as_f64);
            TranscriptEvent::Result {
                success,
                text,
                duration_ms,
                total_cost_usd,
                at,
            }
        }
        _ => TranscriptEvent::Other { raw: value, at },
    }
}

fn parse_assistant(value: &Value, at: DateTime<Utc>) -> TranscriptEvent {
    let mut tool_uses: Vec<ToolUse> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array);
    if let Some(arr) = content {
        for block in arr {
            let bty = block.get("type").and_then(Value::as_str).unwrap_or("");
            match bty {
                "text" => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        text_parts.push(t.to_string());
                    }
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    tool_uses.push(ToolUse {
                        id,
                        name,
                        input,
                        started_at: at,
                    });
                }
                _ => {}
            }
        }
    }
    let usage = value.get("message").and_then(|m| m.get("usage"));
    let tokens_in = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens_out = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    TranscriptEvent::Assistant {
        tool_uses,
        text,
        tokens_in,
        tokens_out,
        at,
    }
}

fn parse_user(value: &Value, at: DateTime<Utc>) -> TranscriptEvent {
    let mut tool_results: Vec<ToolResult> = Vec::new();
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array);
    if let Some(arr) = content {
        for block in arr {
            let bty = block.get("type").and_then(Value::as_str).unwrap_or("");
            if bty == "tool_result" {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let output = block.get("content").cloned().unwrap_or(Value::Null);
                tool_results.push(ToolResult {
                    tool_use_id,
                    output,
                    completed_at: at,
                });
            }
        }
    }
    TranscriptEvent::User { tool_results, at }
}

/// Walk the transcript and find the most recent tool invocation. Returns
/// `None` if there are no `tool_use` blocks anywhere in the stream.
///
/// `now` is the caller's wall-clock at request time — typically
/// `Utc::now()` from the dashboard handler. For an unfinished tool call
/// (no matching `tool_result`) the duration is `now - tool.started_at`,
/// which gives the operator the wall-clock time the tool has been
/// running. For a completed call it's `completed_at - started_at`.
#[must_use]
pub fn extract_last_tool_call(
    events: &[TranscriptEvent],
    now: DateTime<Utc>,
) -> Option<LastToolCall> {
    let mut last: Option<&ToolUse> = None;
    for ev in events {
        if let TranscriptEvent::Assistant { tool_uses, .. } = ev {
            if let Some(tu) = tool_uses.last() {
                last = Some(tu);
            }
        }
    }
    let tu = last?;
    let mut completed_at: Option<DateTime<Utc>> = None;
    for ev in events {
        if let TranscriptEvent::User { tool_results, .. } = ev {
            for tr in tool_results {
                if tr.tool_use_id == tu.id {
                    completed_at = Some(tr.completed_at);
                }
            }
        }
    }
    let end = completed_at.unwrap_or(now);
    let dur = end.signed_duration_since(tu.started_at);
    let secs: u64 = dur.num_seconds().max(0).try_into().unwrap_or(0);
    Some(LastToolCall {
        tool: tu.name.clone(),
        input: tu.input.clone(),
        started_at: tu.started_at,
        duration_so_far_secs: secs,
        completed: completed_at.is_some(),
    })
}

/// Aggregate stats over a transcript.
///
/// `wall_clock_secs` is `last_event_at - first_event_at` derived from the
/// stamped `at` fields, which the caller controls via [`TranscriptTimes`]
/// in [`parse_jsonl_lines`].
#[must_use]
pub fn summarize(events: &[TranscriptEvent]) -> TranscriptSummary {
    let mut tool_histogram: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut first_event_at: Option<DateTime<Utc>> = None;
    let mut last_event_at: Option<DateTime<Utc>> = None;
    for ev in events {
        let at = ev.at();
        if first_event_at.is_none() {
            first_event_at = Some(at);
        }
        last_event_at = Some(at);
        if let TranscriptEvent::Assistant {
            tool_uses,
            tokens_in,
            tokens_out,
            ..
        } = ev
        {
            total_tokens_in = total_tokens_in.saturating_add(*tokens_in);
            total_tokens_out = total_tokens_out.saturating_add(*tokens_out);
            for tu in tool_uses {
                *tool_histogram.entry(tu.name.clone()).or_insert(0) += 1;
            }
        }
    }
    let wall_clock_secs: u64 = match (first_event_at, last_event_at) {
        (Some(a), Some(b)) => b
            .signed_duration_since(a)
            .num_seconds()
            .max(0)
            .try_into()
            .unwrap_or(0),
        _ => 0,
    };
    TranscriptSummary {
        tool_histogram,
        total_tokens_in,
        total_tokens_out,
        wall_clock_secs,
        event_count: events.len() as u64,
        first_event_at,
        last_event_at,
    }
}
