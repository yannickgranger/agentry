//! Renderer + CLI tests for `captain ground`.
//!
//! The renderer test that round-trips the seeds through `AssertionAnchor`
//! is the structural-fence test: if the variants in
//! `orchestrator_types::contract::AssertionAnchor` drift, this test fails
//! either at compile time (variant rename) or at run time (wire-shape
//! divergence).

use orchestrator_runtime::captain_ground::{render_grounding_sheet, CfdbItem, SpecMatch};
use orchestrator_types::contract::AssertionAnchor;
use std::process::Command;

fn item(qname: &str, kind: &str, file: &str, line: u32, ctx: Option<&str>) -> CfdbItem {
    CfdbItem {
        qname: qname.to_string(),
        kind: kind.to_string(),
        file: file.to_string(),
        line,
        bounded_context: ctx.map(str::to_string),
    }
}

fn extract_seeds_json(sheet: &str) -> &str {
    let header = "## Suggested AssertionAnchor seeds";
    let after_header = sheet
        .split_once(header)
        .map(|(_, rest)| rest)
        .expect("seeds header missing");
    let fence_start = after_header
        .find("```json")
        .expect("opening json fence missing");
    let body_start = fence_start + "```json".len();
    let body = &after_header[body_start..];
    let body = body.trim_start_matches('\n');
    let fence_end = body.find("```").expect("closing fence missing");
    body[..fence_end].trim()
}

#[test]
fn render_grounding_sheet_includes_directive() {
    let sheet = render_grounding_sheet("foo bar", &[], &[]);
    assert!(
        sheet.contains("Directive: foo bar"),
        "expected directive line in output, got:\n{sheet}"
    );
}

#[test]
fn render_grounding_sheet_lists_cfdb_items_with_qname_kind_file_line() {
    let items = vec![
        item("crate::foo::bar", "function", "src/foo.rs", 12, Some("foo")),
        item("crate::baz", "struct", "src/baz.rs", 99, None),
    ];
    let sheet = render_grounding_sheet("anything", &items, &[]);
    for it in &items {
        let backticked = format!("`{}`", it.qname);
        assert!(
            sheet.contains(&backticked),
            "expected backtick-wrapped qname `{}` in output\n{sheet}",
            it.qname
        );
        assert!(
            sheet.contains(&it.kind),
            "expected kind `{}` in output\n{sheet}",
            it.kind
        );
        let file_line = format!("{}:{}", it.file, it.line);
        assert!(
            sheet.contains(&file_line),
            "expected file:line `{file_line}` in output\n{sheet}"
        );
    }
}

#[test]
fn render_grounding_sheet_lists_spec_matches_with_file_and_headings() {
    let specs = vec![SpecMatch {
        file: "specs/concepts/foo.md".to_string(),
        headings: vec!["First Heading".to_string(), "Second Heading".to_string()],
    }];
    let sheet = render_grounding_sheet("anything", &[], &specs);
    assert!(
        sheet.contains("specs/concepts/foo.md"),
        "expected spec file path in output:\n{sheet}"
    );
    assert!(
        sheet.contains("First Heading;Second Heading"),
        "expected headings joined by semicolon in output:\n{sheet}"
    );
}

#[test]
fn render_grounding_sheet_emits_assertion_anchor_seeds_for_each_item() {
    let items = vec![
        item("crate::foo", "function", "src/foo.rs", 1, None),
        item("crate::bar", "struct", "src/bar.rs", 2, Some("ctx")),
    ];
    let specs = vec![SpecMatch {
        file: "specs/concepts/baz.md".to_string(),
        headings: vec!["Section A".to_string(), "Section B".to_string()],
    }];
    let sheet = render_grounding_sheet("anything", &items, &specs);
    let json = extract_seeds_json(&sheet);
    let arr: serde_json::Value =
        serde_json::from_str(json).expect("seeds block must parse as JSON");
    let arr = arr.as_array().expect("seeds JSON must be an array");
    assert_eq!(arr.len(), 3, "expected 2 cfdb seeds + 1 spec seed");
    for elem in arr {
        let _: AssertionAnchor = serde_json::from_value(elem.clone())
            .expect("each seed must deserialize as AssertionAnchor");
    }
}

#[test]
fn render_grounding_sheet_handles_empty_items_and_specs() {
    let sheet = render_grounding_sheet("anything", &[], &[]);
    let cfdb_section = sheet
        .split_once("## Candidate cfdb qnames")
        .expect("cfdb header present")
        .1
        .split_once("## Candidate spec sections")
        .expect("spec header present")
        .0;
    assert!(
        cfdb_section.contains("(none)"),
        "cfdb section should print (none) when empty:\n{cfdb_section}"
    );
    let spec_section = sheet
        .split_once("## Candidate spec sections")
        .expect("spec header present")
        .1
        .split_once("## Suggested AssertionAnchor seeds")
        .expect("seeds header present")
        .0;
    assert!(
        spec_section.contains("(none)"),
        "spec section should print (none) when empty:\n{spec_section}"
    );
    let json = extract_seeds_json(&sheet);
    let parsed: Vec<AssertionAnchor> =
        serde_json::from_str(json).expect("empty seeds must parse as Vec<AssertionAnchor>");
    assert!(
        parsed.is_empty(),
        "expected empty seeds vec, got {parsed:?}"
    );
}

#[test]
fn captain_ground_help_lists_required_flags() {
    let bin = env!("CARGO_BIN_EXE_captain");
    let out = Command::new(bin)
        .args(["ground", "--help"])
        .output()
        .expect("spawn captain ground --help");
    assert!(
        out.status.success(),
        "captain ground --help should succeed; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in ["--workspace", "--directive", "--name-pattern"] {
        assert!(
            stdout.contains(flag),
            "expected `{flag}` in help output:\n{stdout}"
        );
    }
}
