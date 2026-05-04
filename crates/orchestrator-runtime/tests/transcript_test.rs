//! Public-surface tests for `transcript.rs`. The broader parsing fixtures
//! live in `tests/transcript_parsing.rs`; this file pins the
//! summarize / extract_last_tool_call time-bearing-fields contract.

use chrono::{Duration, TimeZone, Utc};
use orchestrator_runtime::transcript::{
    extract_last_tool_call, parse_jsonl_lines, summarize, TranscriptEvent, TranscriptTimes,
};

fn fixed_times(span_secs: i64) -> TranscriptTimes {
    let started_at = Utc.with_ymd_and_hms(2026, 4, 27, 10, 0, 0).unwrap();
    TranscriptTimes {
        started_at,
        last_event_at: started_at + Duration::seconds(span_secs),
    }
}

#[test]
fn parse_empty_string_yields_empty_vec() {
    assert!(parse_jsonl_lines("", fixed_times(0)).is_empty());
}

#[test]
fn parse_basic_three_event_fixture() {
    let s = r#"{"type":"system","subtype":"init","session_id":"abc","model":"claude-sonnet-4-7"}
{"type":"assistant","message":{"id":"m","role":"assistant","content":[{"type":"text","text":"pong"}]}}
{"type":"result","subtype":"success","result":"pong","is_error":false,"duration_ms":420}
"#;
    let events = parse_jsonl_lines(s, fixed_times(7));
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], TranscriptEvent::SystemInit { .. }));
    assert!(matches!(events[1], TranscriptEvent::Assistant { .. }));
    assert!(matches!(events[2], TranscriptEvent::Result { .. }));
}

#[test]
fn unfinished_tool_call_has_nonzero_duration_when_now_after_last_event() {
    // Regression-lock for the v4 reviewer blocker: durations and
    // wall_clock_secs must be derived from caller-supplied wall-clock,
    // not from a single `Utc::now()` read inside the parser.
    let s = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"id":"m","role":"assistant","content":[{"type":"tool_use","id":"tu","name":"Bash","input":{"command":"sleep 60"}}]}}
"#;
    let times = fixed_times(20);
    let now = times.last_event_at + Duration::seconds(13);

    let events = parse_jsonl_lines(s, times);
    let summary = summarize(&events);
    assert_eq!(
        summary.wall_clock_secs, 20,
        "wall_clock_secs must reflect caller-provided last - first"
    );
    assert_eq!(summary.first_event_at, Some(times.started_at));
    assert_eq!(summary.last_event_at, Some(times.last_event_at));

    let last = extract_last_tool_call(&events, now).expect("a tool_use exists");
    assert!(!last.completed, "no tool_result yet → still running");
    assert_eq!(
        last.duration_so_far_secs, 13,
        "duration_so_far_secs must be now - tool.started_at, not zero"
    );
}
