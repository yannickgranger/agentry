//! Pre-commit dead-pub gate. Reads `{diff, workspace_root}` JSON on stdin,
//! parses unified-diff text for newly-added `pub` items, queries `ra-query
//! callers <name>` for each, and emits one JSONL finding per item with zero
//! callers plus a summary line. Behaviour-preserving port of the bash gate
//! removed in PR #133's follow-up; the binary form gives file:line accuracy
//! from the diff hunk headers and structural immunity to the empty-grep
//! pipefail failure class that bit PRs #129/#130/#135.

use std::io::{self, BufWriter, Read, Write};
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Input {
    diff: String,
    #[serde(default)]
    #[allow(dead_code)]
    workspace_root: String,
}

#[derive(Debug, PartialEq, Eq)]
struct AddedPubItem {
    file: String,
    line: u32,
    kind: String,
    name: String,
}

fn parse_added_pub_items(diff: &str) -> Vec<AddedPubItem> {
    let mut items = Vec::new();
    let mut current_file: Option<String> = None;
    let mut new_line: u32 = 0;
    let mut in_hunk = false;

    for raw in diff.lines() {
        if let Some(rest) = raw.strip_prefix("+++ b/") {
            current_file = Some(rest.to_string());
            in_hunk = false;
            continue;
        }
        if raw.starts_with("+++ ") || raw.starts_with("--- ") {
            in_hunk = false;
            continue;
        }
        if let Some(rest) = raw.strip_prefix("@@") {
            if let Some(line) = parse_hunk_new_start(rest) {
                new_line = line;
                in_hunk = true;
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(body) = raw.strip_prefix('+') {
            if let (Some(file), Some((kind, name))) = (current_file.as_ref(), match_pub_item(body))
            {
                items.push(AddedPubItem {
                    file: file.clone(),
                    line: new_line,
                    kind: kind.to_string(),
                    name: name.to_string(),
                });
            }
            new_line += 1;
        } else if raw.starts_with('-') {
            // deletion line — does not advance new-file line counter
        } else {
            // context line (or empty) — advances new-file counter
            new_line += 1;
        }
    }
    items
}

fn parse_hunk_new_start(rest: &str) -> Option<u32> {
    let plus_idx = rest.find('+')?;
    let after = &rest[plus_idx + 1..];
    let end = after.find([',', ' ']).unwrap_or(after.len());
    after[..end].parse::<u32>().ok()
}

fn match_pub_item(body: &str) -> Option<(&'static str, &str)> {
    let trimmed = body.trim_start();
    let after_pub = trimmed.strip_prefix("pub ")?;
    let after_pub = after_pub.trim_start();
    for kind in ["fn", "struct", "enum", "trait", "type", "const", "static"] {
        if let Some(rest) = after_pub.strip_prefix(kind) {
            let next = rest.chars().next()?;
            if !next.is_whitespace() {
                continue;
            }
            let rest = rest.trim_start();
            let name_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if name_end == 0 {
                return None;
            }
            let name = &rest[..name_end];
            let first = name.chars().next()?;
            if !(first.is_ascii_alphabetic() || first == '_') {
                return None;
            }
            return Some((kind, name));
        }
    }
    None
}

fn callers_for(name: &str) -> Result<usize, RaQueryStatus> {
    let output = match Command::new("ra-query")
        .args(["callers", name, "--format", "json"])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(RaQueryStatus::NotFound),
        Err(_) => return Ok(0),
    };
    if !output.status.success() {
        return Ok(0);
    }
    let v: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return Ok(0),
    };
    let len = v
        .get("callers")
        .and_then(|c| c.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    Ok(len)
}

enum RaQueryStatus {
    NotFound,
}

fn run<W: Write>(input: Input, mut out: W) -> io::Result<()> {
    let items = parse_added_pub_items(&input.diff);
    let total = items.len();
    let mut zero_callers = 0usize;
    for item in &items {
        match callers_for(&item.name) {
            Err(RaQueryStatus::NotFound) => {
                writeln!(
                    out,
                    "{}",
                    serde_json::json!({
                        "severity": "info",
                        "category": "ra-query-unavailable",
                        "message": "ra-query binary not on PATH; skipping dead-pub gate"
                    })
                )?;
                return Ok(());
            }
            Ok(0) => {
                zero_callers += 1;
                writeln!(
                    out,
                    "{}",
                    serde_json::json!({
                        "severity": "warn",
                        "category": "dead-pub",
                        "file": item.file,
                        "line": item.line,
                        "message": format!(
                            "newly-added pub {} '{}' has zero callers in workspace",
                            item.kind, item.name
                        ),
                    })
                )?;
            }
            Ok(_) => {}
        }
    }
    writeln!(
        out,
        "{}",
        serde_json::json!({
            "severity": "info",
            "category": "dead-pub-summary",
            "message": format!(
                "checked {} newly-added pub items, {} with zero callers",
                total, zero_callers
            ),
        })
    )?;
    Ok(())
}

fn main() {
    let mut buf = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut buf) {
        eprintln!("dead-pub-check: failed to read stdin: {e}");
        std::process::exit(1);
    }
    let input: Input = match serde_json::from_str(&buf) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("dead-pub-check: malformed stdin JSON: {e}");
            std::process::exit(1);
        }
    };
    let stdout = io::stdout();
    let writer = BufWriter::new(stdout.lock());
    if let Err(e) = run(input, writer) {
        eprintln!("dead-pub-check: write error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_diff_pub_fn() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -0,0 +1,1 @@
+pub fn foo() {}
";
        let items = parse_added_pub_items(diff);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "fn");
        assert_eq!(items[0].name, "foo");
        assert_eq!(items[0].file, "src/lib.rs");
        assert_eq!(items[0].line, 1);
    }

    #[test]
    fn parses_pub_struct_and_enum() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,0 +1,3 @@
+pub struct Foo;
+pub enum Bar { A }
+pub fn baz() {}
";
        let items = parse_added_pub_items(diff);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, "struct");
        assert_eq!(items[0].name, "Foo");
        assert_eq!(items[1].kind, "enum");
        assert_eq!(items[1].name, "Bar");
        assert_eq!(items[2].kind, "fn");
        assert_eq!(items[2].name, "baz");
    }

    #[test]
    fn tracks_file_and_line_correctly() {
        let diff = "\
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,0 +11,1 @@
+pub fn alpha() {}
--- a/src/b.rs
+++ b/src/b.rs
@@ -50,0 +51,2 @@
+pub fn beta() {}
+pub fn gamma() {}
";
        let items = parse_added_pub_items(diff);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].file, "src/a.rs");
        assert_eq!(items[0].line, 11);
        assert_eq!(items[0].name, "alpha");
        assert_eq!(items[1].file, "src/b.rs");
        assert_eq!(items[1].line, 51);
        assert_eq!(items[1].name, "beta");
        assert_eq!(items[2].file, "src/b.rs");
        assert_eq!(items[2].line, 52);
        assert_eq!(items[2].name, "gamma");
    }

    #[test]
    fn v1_emits_pub_inside_test_module_lacking_semantic_awareness() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,0 +1,5 @@
+#[cfg(test)]
+mod tests {
+    pub fn helper() {}
+}
+
";
        let items = parse_added_pub_items(diff);
        assert_eq!(
            items.len(),
            1,
            "v1 has no semantic awareness — emits pub-in-test-module"
        );
        assert_eq!(items[0].name, "helper");
    }

    #[test]
    fn extracts_zero_added_pub_items_from_empty_diff() {
        let items = parse_added_pub_items("");
        assert_eq!(items.len(), 0);
    }

    #[test]
    fn ignores_pub_in_context_or_deletion_lines() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 pub fn pre_existing() {}
-pub fn old() {}
+pub fn new() {}
";
        let items = parse_added_pub_items(diff);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "new");
    }

    #[test]
    fn parses_all_seven_kinds() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -0,0 +1,7 @@
+pub fn f() {}
+pub struct S;
+pub enum E {}
+pub trait T {}
+pub type Y = u32;
+pub const C: u32 = 0;
+pub static X: u32 = 0;
";
        let items = parse_added_pub_items(diff);
        assert_eq!(items.len(), 7);
        let kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["fn", "struct", "enum", "trait", "type", "const", "static"]
        );
    }
}
