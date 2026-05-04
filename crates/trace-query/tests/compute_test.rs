use serde_json::json;
use trace_query::compute::*;
use trace_query::TraceMetric;

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
fn trace_metric_default_round_trips() {
    let m = TraceMetric::default();
    let s = serde_json::to_string(&m).expect("ser");
    let back: TraceMetric = serde_json::from_str(&s).expect("de");
    assert_eq!(m, back);
}

#[test]
fn trace_metric_deserialises_with_missing_fields() {
    let m: TraceMetric = serde_json::from_str("{\"brief_id\":\"x\"}").expect("de");
    assert_eq!(m.brief_id, "x");
    assert_eq!(m.compile_cycles, 0);
}
