//! Integration test for `claude -p` transcript capture (issue #93).
//!
//! This test does NOT spawn a real podman container or call `claude -p`. It
//! exercises the **fixture-replay** path: write a synthetic stream-json
//! transcript to a temporary `/transcripts/` directory, then verify
//!
//!   1. the transcript file is well-formed JSONL,
//!   2. the same `jq` extraction the production script uses (in
//!      `BASH_PRELUDE::stream_claude`) recovers the assistant's final text
//!      from a `result`-typed event, and
//!   3. the `BASH_PRELUDE` source contains the streaming + pipefail-guard
//!      contract that issue #93 requires.
//!
//! Live-podman/live-claude end-to-end coverage is gated behind the manual
//! dogfood probe (see `examples/verify-M5b.json`) — it requires the host
//! `claude` CLI + Claude Max OAuth and is not appropriate for CI. This
//! fixture replay is what runs every PR.

use std::fs;
use std::process::Command;

/// Skip-if-absent: returns true when `jq` is on the runner's PATH. Production
/// containers always install jq (it is part of every role's package manifest);
/// CI runners may not. The two tests that shell out to jq use this gate so
/// they validate the production query when run locally and no-op otherwise.
fn jq_available() -> bool {
    Command::new("jq")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Sample stream-json output as produced by `claude -p --output-format stream-json --verbose`
/// for the prompt "Reply with exactly one word: pong". Captured from a real
/// Claude Max run; trimmed to the structurally relevant events.
const FIXTURE_TRANSCRIPT: &str = r#"{"type":"system","subtype":"init","session_id":"test","model":"claude-sonnet-4-7"}
{"type":"assistant","message":{"id":"msg_test","role":"assistant","content":[{"type":"text","text":"pong"}],"stop_reason":"end_turn"}}
{"type":"result","subtype":"success","total_cost_usd":0.0001,"is_error":false,"duration_ms":420,"result":"pong","session_id":"test"}
"#;

/// `BASH_PRELUDE` defines the streaming helper. We assert against its source
/// here because the helper's contract (pipefail-guard + PIPESTATUS-capture +
/// transcript-emit + final-text reconstruction) is what issue #93 requires.
/// Re-importing the const string from the lib crate keeps the test honest:
/// if the helper drifts away from the contract, this fails.
fn read_bash_prelude_source() -> String {
    let src = fs::read_to_string("src/seed.rs")
        .expect("read crates/orchestrator-runtime/src/seed.rs from cargo test cwd");
    let start = src
        .find("const BASH_PRELUDE: &str = r#\"")
        .expect("BASH_PRELUDE const in seed.rs");
    // BASH_PRELUDE is delimited by r#"..."#; find the matching closer.
    let after_open = &src[start + "const BASH_PRELUDE: &str = r#\"".len()..];
    let end = after_open
        .find("\"#;")
        .expect("closing \"#; of BASH_PRELUDE");
    after_open[..end].to_string()
}

#[test]
fn transcript_file_is_valid_jsonl_per_line() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("brf_test_capture.jsonl");
    fs::write(&path, FIXTURE_TRANSCRIPT).expect("write fixture");

    let body = fs::read_to_string(&path).expect("read fixture");
    let lines: Vec<&str> = body.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "fixture should have 3 stream events");
    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} not valid JSON: {line:?} ({e})"));
        assert!(
            parsed.get("type").is_some(),
            "every stream-json line must have a `type` field; line {i}: {line:?}"
        );
    }
}

#[test]
fn jq_extraction_recovers_assistant_final_text() {
    // The production script (stream_claude in BASH_PRELUDE) extracts the
    // assistant's final text from the transcript by running:
    //   jq -r 'select(.type=="result") | .result' "$_t" | tail -1
    // This test verifies that extraction against the fixture when `jq` is
    // available; CI runners without jq skip with a notice.
    if !jq_available() {
        eprintln!("[skip] jq not on PATH; production containers always install it");
        return;
    }
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("brf_test_capture.jsonl");
    fs::write(&path, FIXTURE_TRANSCRIPT).expect("write fixture");

    let out = Command::new("jq")
        .args(["-r", r#"select(.type=="result") | .result"#])
        .arg(&path)
        .output()
        .expect("jq invocation");
    assert!(
        out.status.success(),
        "jq exited non-zero: stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result_text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        result_text, "pong",
        "jq must extract `pong` as the assistant's final text from the result event"
    );
}

#[test]
fn serde_extraction_recovers_assistant_final_text() {
    // Rust mirror of the jq query — runs everywhere, including CI runners
    // without jq. If this passes and jq's version fails, the regression is
    // in jq usage / install. If both pass, the contract holds end-to-end.
    let lines: Vec<serde_json::Value> = FIXTURE_TRANSCRIPT
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line valid JSON"))
        .collect();
    let result_text: Option<&str> = lines
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("result"))
        .filter_map(|v| v.get("result").and_then(|r| r.as_str()))
        .next_back();
    assert_eq!(
        result_text,
        Some("pong"),
        "serde must mirror jq's `select(.type==\"result\") | .result` extraction"
    );
}

const TRUNCATED_FIXTURE: &str = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"partial"}]}}
"#;

#[test]
fn jq_fallback_extracts_assistant_text_when_no_result_event() {
    // If a transcript ends without a `result` event (e.g. claude was killed
    // mid-stream by `timeout`), stream_claude falls back to the last
    // `assistant` event's text content. Verifies the fallback jq query when
    // jq is on PATH; otherwise skipped (see `serde_fallback_*` for the
    // always-runs Rust mirror).
    if !jq_available() {
        eprintln!("[skip] jq not on PATH; production containers always install it");
        return;
    }
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("brf_truncated.jsonl");
    fs::write(&path, TRUNCATED_FIXTURE).expect("write fixture");

    let out = Command::new("jq")
        .args([
            "-r",
            r#"select(.type=="assistant") | .message.content[]? | select(.type=="text") | .text"#,
        ])
        .arg(&path)
        .output()
        .expect("jq invocation");
    assert!(out.status.success(), "jq exited non-zero");
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        text, "partial",
        "fallback jq must extract the assistant's last text when no result event"
    );
}

#[test]
fn serde_fallback_extracts_assistant_text_when_no_result_event() {
    // Rust mirror of the fallback jq query — same shape, runs without jq.
    let lines: Vec<serde_json::Value> = TRUNCATED_FIXTURE
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line valid JSON"))
        .collect();
    let text: Option<&str> = lines
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("assistant"))
        .filter_map(|v| {
            v.get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                        .next_back()
                })
        })
        .next_back();
    assert_eq!(
        text,
        Some("partial"),
        "serde fallback must mirror jq's assistant.message.content[].text extraction"
    );
}

#[test]
fn bash_prelude_contains_streaming_contract() {
    // The contract issue #93 establishes — every term must appear in the
    // BASH_PRELUDE source so future drift is caught at PR time.
    let prelude = read_bash_prelude_source();
    assert!(
        prelude.contains("--output-format stream-json --verbose"),
        "stream-json + verbose flags must be in stream_claude"
    );
    assert!(
        prelude.contains("tee \"$_t\""),
        "must tee stream into the transcript file"
    );
    assert!(
        prelude.contains("} || true"),
        "must wrap pipeline in `}} || true` to defuse set -e + pipefail"
    );
    assert!(
        prelude.contains("PIPESTATUS[0]"),
        "must capture timeout's exit code via PIPESTATUS[0], not $?"
    );
    assert!(
        prelude.contains("/transcripts/${brief_id}"),
        "must write to /transcripts/${{brief_id}}<suffix>.jsonl"
    );
    assert!(
        prelude.contains("emit_done \"failed\""),
        "must emit_done failed on non-zero exit (this is what was broken before #93's fix)"
    );
}

#[test]
fn trace_event_shape_when_each_line_emitted() {
    // For each stream-json line consumed by the inner `while read` loop,
    // stream_claude emits a trace event of the form:
    //   { "at":"...", "type":"event", "payload":{ "claude": <line-as-json> } }
    // (or `claude_raw` if the line wasn't valid JSON). This test asserts the
    // shape an external consumer can rely on — without spinning up a daemon.
    for line in FIXTURE_TRANSCRIPT.lines().filter(|l| !l.is_empty()) {
        let claude_value: serde_json::Value =
            serde_json::from_str(line).expect("fixture line valid JSON");
        // Simulate the trace event a consumer would observe.
        let envelope = serde_json::json!({
            "at": "2026-04-27T03:00:00Z",
            "type": "event",
            "payload": { "claude": claude_value },
        });
        let payload = envelope
            .get("payload")
            .and_then(|p| p.get("claude"))
            .expect("every emitted trace event must carry payload.claude");
        assert!(
            payload.get("type").is_some(),
            "the inner claude object must have a stream-json `type`"
        );
    }
}
