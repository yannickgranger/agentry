//! Unit tests for the `transcript` module's parser + summarizer.
//!
//! These exercise the pure parsing surface only — no I/O, no daemon, no
//! container. The dashboard's HTTP routes get integration coverage in the
//! `orchestrator-dashboard` crate.

use orchestrator_runtime::transcript::{
    extract_last_tool_call, parse_jsonl_lines, summarize, TranscriptEvent,
};

#[test]
fn empty_input_returns_empty_vec() {
    assert!(parse_jsonl_lines("").is_empty());
    assert!(parse_jsonl_lines("\n\n").is_empty());
}

#[test]
fn truncated_last_line_dropped() {
    let s = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"id":"m""#;
    let events = parse_jsonl_lines(s);
    assert_eq!(events.len(), 1, "only the well-formed line survives");
    assert!(matches!(events[0], TranscriptEvent::SystemInit { .. }));
}

#[test]
fn multi_turn_with_thinking_blocks() {
    let s = r#"{"type":"system","subtype":"init","session_id":"s","model":"claude-sonnet-4-7"}
{"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"let me try"}],"usage":{"input_tokens":10,"output_tokens":5}}}
{"type":"assistant","message":{"id":"m2","role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{"path":"/etc/hosts"}}],"usage":{"input_tokens":12,"output_tokens":3}}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"127.0.0.1 localhost"}]}}
{"type":"result","subtype":"success","result":"done","is_error":false,"total_cost_usd":0.0042,"duration_ms":1234}
"#;
    let events = parse_jsonl_lines(s);
    assert_eq!(events.len(), 5);
    match &events[1] {
        TranscriptEvent::Assistant {
            text,
            tool_uses,
            tokens_in,
            ..
        } => {
            assert_eq!(text.as_deref(), Some("let me try"));
            assert!(tool_uses.is_empty());
            assert_eq!(*tokens_in, 10);
        }
        other => panic!("expected Assistant turn 1, got {other:?}"),
    }
    match &events[2] {
        TranscriptEvent::Assistant { tool_uses, .. } => {
            assert_eq!(tool_uses.len(), 1);
            assert_eq!(tool_uses[0].name, "Read");
        }
        other => panic!("expected Assistant turn 2, got {other:?}"),
    }
    match &events[3] {
        TranscriptEvent::User { tool_results, .. } => {
            assert_eq!(tool_results.len(), 1);
            assert_eq!(tool_results[0].tool_use_id, "tu_1");
        }
        other => panic!("expected User turn, got {other:?}"),
    }
    match &events[4] {
        TranscriptEvent::Result { success, text, .. } => {
            assert!(*success);
            assert_eq!(text.as_deref(), Some("done"));
        }
        other => panic!("expected Result, got {other:?}"),
    }
}

#[test]
fn tool_use_without_matching_result_is_running() {
    let s = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"id":"m","role":"assistant","content":[{"type":"tool_use","id":"tu_run","name":"Bash","input":{"command":"sleep 30"}}]}}
"#;
    let events = parse_jsonl_lines(s);
    let last = extract_last_tool_call(&events).expect("a tool_use exists");
    assert_eq!(last.tool, "Bash");
    assert!(!last.completed, "no tool_result yet → running");
    assert_eq!(
        last.input.get("command").and_then(|v| v.as_str()),
        Some("sleep 30")
    );
}

#[test]
fn summarize_token_accounting() {
    let s = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"a","name":"Read","input":{}}],"usage":{"input_tokens":100,"output_tokens":20}}}
{"type":"assistant","message":{"id":"m2","content":[{"type":"tool_use","id":"b","name":"Read","input":{}},{"type":"tool_use","id":"c","name":"Bash","input":{}}],"usage":{"input_tokens":50,"output_tokens":15}}}
{"type":"result","subtype":"success","result":"done","is_error":false}
"#;
    let events = parse_jsonl_lines(s);
    let summary = summarize(&events);
    assert_eq!(summary.event_count, 4);
    assert_eq!(summary.total_tokens_in, 150);
    assert_eq!(summary.total_tokens_out, 35);
    assert_eq!(summary.tool_histogram.get("Read").copied(), Some(2));
    assert_eq!(summary.tool_histogram.get("Bash").copied(), Some(1));
}
