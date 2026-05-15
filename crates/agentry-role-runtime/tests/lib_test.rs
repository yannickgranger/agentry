//! Public-surface tests migrated from `src/lib.rs`'s prior inline
//! `#[cfg(test)] mod tests` block.
//!
//! Tests that exercised crate-private helpers (`verdict_to_str`,
//! `DONE_EMITTED`, `REFUSAL_COUNT`, `DoneGuard` flag inspection,
//! `column_for_name_at_line`, `difference`, `classify_new_pub_items`,
//! `callers_zero_finding`, `callers_unresolved_finding`, the `PubItem`
//! struct) are dropped here — the migration recipe forbids promoting
//! their visibility, and the behaviour is exercised end-to-end by the
//! synthetic-repo integration tests in `tests/run_fence_test.rs`.

use agentry_role_runtime::{
    changed_rs_files, drop_empty_blocker_findings, head_bytes, mech_finding, parse_allowed_tools,
    parse_findings, parse_severity, pointer_str_or, slice_json_array, slice_last_json_array,
    strip_fences, tail_bytes, tail_lines,
};
use chrono::Utc;
use orchestrator_types::{Event, EventKind, FindingOrigin, Severity};
use serde_json::{json, Value};

#[test]
fn pointer_str_or_uses_default_when_missing_or_empty() {
    let v = json!({"a": "value", "b": ""});
    assert_eq!(pointer_str_or(&v, "/missing", "default"), "default");
    assert_eq!(pointer_str_or(&v, "/a", "default"), "value");
    assert_eq!(pointer_str_or(&v, "/b", "default"), "default");
}

#[test]
fn parse_severity_known_blocker() {
    assert_eq!(parse_severity(Some("blocker")), Severity::Blocker);
}

#[test]
fn parse_severity_unknown_defaults_to_warn() {
    assert_eq!(parse_severity(Some("warn")), Severity::Warn);
    assert_eq!(parse_severity(Some("info")), Severity::Warn);
    assert_eq!(parse_severity(None), Severity::Warn);
}

#[test]
fn head_bytes_respects_char_boundary() {
    // Emoji is 4 bytes in UTF-8. Budget 5 should clamp to 4 (after the
    // emoji) or 0 (before), never split it.
    let s = "🚀x"; // 4 bytes + 1 byte = 5 bytes
    let h = head_bytes(s, 4);
    assert!(h.is_empty() || h == "🚀");
}

#[test]
fn tail_lines_returns_last_n() {
    let buf = b"a\nb\nc\nd\ne";
    assert_eq!(tail_lines(buf, 3), "c\nd\ne");
    assert_eq!(tail_lines(buf, 100), "a\nb\nc\nd\ne");
    assert_eq!(tail_lines(b"", 5), "");
}

#[test]
fn tail_bytes_returns_last_n() {
    let buf = b"abcdefgh";
    assert_eq!(tail_bytes(buf, 3), "fgh");
    assert_eq!(tail_bytes(buf, 100), "abcdefgh");
}

#[test]
fn strip_fences_drops_json_and_plain() {
    assert_eq!(strip_fences("```json\n{\"x\":1}\n```\n"), "{\"x\":1}\n");
}

#[test]
fn strip_fences_drops_json_and_plain_fences() {
    let raw = "```json\n[1,2]\n```\n";
    assert_eq!(strip_fences(raw), "[1,2]\n");
}

#[test]
fn strip_fences_drops_blank_lines() {
    let raw = "\n[1,2]\n\nmore\n";
    assert_eq!(strip_fences(raw), "[1,2]\nmore\n");
}

#[test]
fn emit_tool_refused_round_trips_into_event_kind_tool_refused() {
    // Build the same JSON object emit_tool_refused writes and parse it
    // through the typed Event enum — this exercises the wire shape
    // without touching stdout.
    let line = json!({
        "at": Utc::now().to_rfc3339(),
        "type": "tool_refused",
        "tool": "Bash",
        "command": "rm -rf /",
    });
    let s = serde_json::to_string(&line).expect("ser");
    let evt: Event = serde_json::from_str(&s).expect("de");
    match evt.kind {
        EventKind::ToolRefused { tool, command } => {
            assert_eq!(tool, "Bash");
            assert_eq!(command.as_deref(), Some("rm -rf /"));
        }
        other => panic!("expected ToolRefused, got {other:?}"),
    }
}

#[test]
fn emit_tool_refused_with_none_command_round_trips() {
    let line = json!({
        "at": Utc::now().to_rfc3339(),
        "type": "tool_refused",
        "tool": "Read",
        "command": Value::Null,
    });
    let s = serde_json::to_string(&line).expect("ser");
    let evt: Event = serde_json::from_str(&s).expect("de");
    match evt.kind {
        EventKind::ToolRefused { tool, command } => {
            assert_eq!(tool, "Read");
            assert!(command.is_none());
        }
        other => panic!("expected ToolRefused, got {other:?}"),
    }
}

#[test]
fn parse_allowed_tools_reads_string_array() {
    let bundle = json!({
        "permit": {"allowed_tools": ["Read", "Edit", "Bash(cargo fmt:*)"]},
    });
    assert_eq!(
        parse_allowed_tools(&bundle),
        vec![
            "Read".to_string(),
            "Edit".to_string(),
            "Bash(cargo fmt:*)".to_string()
        ]
    );
}

#[test]
fn parse_allowed_tools_returns_empty_when_missing() {
    let bundle = json!({"permit": {}});
    assert!(parse_allowed_tools(&bundle).is_empty());
}

#[test]
fn parse_allowed_tools_returns_empty_for_non_array() {
    let bundle = json!({"permit": {"allowed_tools": "not an array"}});
    assert!(parse_allowed_tools(&bundle).is_empty());
}

#[test]
fn parse_allowed_tools_drops_non_string_entries() {
    let bundle = json!({"permit": {"allowed_tools": ["Read", 42, null, "Edit"]}});
    assert_eq!(
        parse_allowed_tools(&bundle),
        vec!["Read".to_string(), "Edit".to_string()]
    );
}

#[test]
fn mech_finding_uses_blocker_severity() {
    let f = mech_finding("cargo-fmt", "fmt", "boom");
    assert!(matches!(f.severity, Severity::Blocker));
    match &f.origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, "cargo-fmt");
            assert!(rule.is_none());
        }
        _ => panic!("expected mechanical origin"),
    }
    assert_eq!(f.category, "fmt");
    assert_eq!(f.message, "boom");
}

#[test]
fn slice_json_array_picks_outermost() {
    assert_eq!(slice_json_array("prefix[1,2]suffix"), Some("[1,2]"));
    assert_eq!(
        slice_json_array("garbage [\"a\",\"b\"]"),
        Some("[\"a\",\"b\"]")
    );
}

#[test]
fn slice_json_array_returns_none_when_missing() {
    assert_eq!(slice_json_array("no brackets here"), None);
    assert_eq!(slice_json_array("] only closing"), None);
    assert_eq!(slice_json_array("[ only opening"), None);
}

#[test]
fn parse_findings_empty_array_yields_zero() {
    let v = parse_findings("[]", "agt-1");
    assert_eq!(v.len(), 0);
}

#[test]
fn parse_findings_extracts_blocker_with_constraints() {
    let response = r#"
    ```json
    [
      {
        "severity": "blocker",
        "category": "invariant",
        "message": "missing idempotency gate",
        "prohibitions": ["do not introduce a new abstraction"],
        "requirements": ["wrap emission in SETNX"]
      },
      {
        "severity": "warn",
        "category": "naming",
        "message": "function could be renamed"
      }
    ]
    ```
    "#;
    let v = parse_findings(response, "agt-2");
    assert_eq!(v.len(), 2);
    assert!(matches!(v[0].severity, Severity::Blocker));
    assert_eq!(v[0].category, "invariant");
    assert_eq!(v[0].prohibitions.len(), 1);
    assert_eq!(v[0].requirements.len(), 1);
    assert!(matches!(v[1].severity, Severity::Warn));
    assert_eq!(v[1].prohibitions.len(), 0);
    match &v[0].origin {
        FindingOrigin::Model { reviewer_agent_id } => {
            assert_eq!(reviewer_agent_id, "agt-2");
        }
        _ => panic!("expected Model origin"),
    }
}

#[test]
fn parse_findings_salvages_prose_only_reply() {
    let v = parse_findings("I think this looks fine actually.", "agt-3");
    assert_eq!(v.len(), 1);
    assert!(matches!(v[0].severity, Severity::Blocker));
    assert_eq!(v[0].category, "format_deviation");
    assert!(v[0].message.contains("looks fine"));
}

#[test]
fn parse_findings_salvages_when_brackets_dont_form_array() {
    // `]xyz[` has both brackets but slice_json_array returns None
    // because end < start.
    let v = parse_findings("] not actually an array [", "agt-4");
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].category, "format_deviation");
}

#[test]
fn parse_findings_salvages_when_sliced_text_is_not_array() {
    // First-bracket-to-last-bracket happens to slice an object instead
    // of an array — ensure salvage fires rather than panicking.
    let v = parse_findings("prelude [{\"x\":1}] postscript", "agt-5");
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].category, "other"); // {x:1} has no category — defaults
}

#[test]
fn parse_findings_handles_prose_with_brackets_then_empty_array() {
    // Brief 477 reproduction: prose containing literal `[]` brackets BEFORE
    // a clean trailing JSON array. The first-pass slice captures the whole
    // prose (which fails to parse); the line-anchored fallback finds the
    // trailing `[]` and the verdict correctly resolves to zero findings.
    let response = "[ignore]'d. `noeviction` is the correct policy.\nNo concerns triggered.\n[]";
    let v = parse_findings(response, "agt-477");
    assert_eq!(v.len(), 0);
}

#[test]
fn parse_findings_handles_prose_with_brackets_then_real_findings() {
    let response = "Some [ignored] prose mentioning [arrays] in text.\n\
        [{\"severity\":\"blocker\",\"category\":\"foo\",\"message\":\"bar\"}]";
    let v = parse_findings(response, "agt-fb-real");
    assert_eq!(v.len(), 1);
    assert!(matches!(v[0].severity, Severity::Blocker));
    assert_eq!(v[0].category, "foo");
    assert_eq!(v[0].message, "bar");
}

#[test]
fn parse_findings_unchanged_for_clean_array_at_start() {
    // Existing parse_findings_empty_array_yields_zero behavior preserved.
    let v = parse_findings("[]", "agt-clean");
    assert_eq!(v.len(), 0);
}

#[test]
fn parse_findings_unchanged_for_clean_array_with_fences() {
    // strip_fences runs first, so the fenced empty array still parses on
    // the primary path.
    let v = parse_findings("```json\n[]\n```", "agt-fenced");
    assert_eq!(v.len(), 0);
}

#[test]
fn slice_last_json_array_finds_trailing_array() {
    assert_eq!(
        slice_last_json_array("preamble [foo] more text\n[{\"x\":1}]"),
        Some("[{\"x\":1}]")
    );
    assert_eq!(slice_last_json_array("no array here"), None);
    // `[` at byte position 0 satisfies the line-anchored rule.
    assert_eq!(slice_last_json_array("[]"), Some("[]"));
}

#[test]
fn drop_empty_blocker_findings_drops_only_all_empty_blockers() {
    // #311 fence: a Blocker whose message+requirements+prohibitions are
    // all empty is a parse failure (no actionable signal), so the
    // reviewer must drop it; the surviving verdict downgrades to
    // Shipped because no real Blockers remain.
    let response = r#"
    [
      {
        "severity": "blocker",
        "category": "other",
        "message": "",
        "prohibitions": [],
        "requirements": []
      }
    ]
    "#;
    let mut findings = parse_findings(response, "agt-empty");
    assert_eq!(findings.len(), 1);
    assert!(matches!(findings[0].severity, Severity::Blocker));
    let dropped = drop_empty_blocker_findings(&mut findings);
    assert_eq!(dropped, 1, "the all-empty Blocker must be dropped");
    assert_eq!(findings.len(), 0, "no findings should survive");
    // With no surviving Blocker, the runner emits Shipped — pinning the
    // downgrade contract: empty Blocker -> ReworkNeeded -> Shipped.
    let still_has_blocker = findings
        .iter()
        .any(|f| matches!(f.severity, Severity::Blocker));
    assert!(
        !still_has_blocker,
        "Shipped path requires no surviving Blocker"
    );
}

#[test]
fn drop_empty_blocker_findings_keeps_real_blockers_and_warns() {
    // A Blocker with a non-empty message stays. A Warn with all-empty
    // fields stays — the fence only targets empty BLOCKERS, not Warns.
    let response = r#"
    [
      {
        "severity": "blocker",
        "category": "invariant",
        "message": "real defect",
        "prohibitions": [],
        "requirements": []
      },
      {
        "severity": "warn",
        "category": "naming",
        "message": "",
        "prohibitions": [],
        "requirements": []
      },
      {
        "severity": "blocker",
        "category": "other",
        "message": "",
        "prohibitions": [],
        "requirements": []
      }
    ]
    "#;
    let mut findings = parse_findings(response, "agt-mixed");
    assert_eq!(findings.len(), 3);
    let dropped = drop_empty_blocker_findings(&mut findings);
    assert_eq!(dropped, 1, "only the all-empty Blocker should be dropped");
    assert_eq!(findings.len(), 2);
    let still_has_blocker = findings
        .iter()
        .any(|f| matches!(f.severity, Severity::Blocker));
    assert!(still_has_blocker, "real Blocker must survive");
}

#[test]
fn drop_empty_blocker_findings_keeps_blocker_with_only_requirements() {
    // Any one of message / requirements / prohibitions being non-empty
    // is enough to keep the Blocker — only the all-three-empty case is
    // a parse failure.
    let response = r#"
    [
      {
        "severity": "blocker",
        "category": "other",
        "message": "",
        "prohibitions": [],
        "requirements": ["use SETNX"]
      }
    ]
    "#;
    let mut findings = parse_findings(response, "agt-req-only");
    assert_eq!(findings.len(), 1);
    let dropped = drop_empty_blocker_findings(&mut findings);
    assert_eq!(dropped, 0);
    assert_eq!(findings.len(), 1);
}

#[test]
fn changed_rs_files_picks_only_rust() {
    let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                --- a/src/lib.rs\n\
                +++ b/src/lib.rs\n\
                @@ -1 +1 @@\n\
                -old\n\
                +new\n\
                diff --git a/notes.md b/notes.md\n\
                --- a/notes.md\n\
                +++ b/notes.md\n\
                @@ -1 +1 @@\n\
                -a\n\
                +b\n";
    let v = changed_rs_files(diff);
    assert_eq!(v, vec!["src/lib.rs"]);
}
