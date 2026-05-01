//! Pre-commit dead-pub gate. Reads `{diff, workspace_root}` JSON on stdin,
//! parses unified-diff text for newly-added `pub` items, queries `ra-query
//! callers <name>` for each, and emits one JSONL finding per item with zero
//! callers plus a summary line. Behaviour-preserving port of the bash gate
//! removed in PR #133's follow-up; the binary form gives file:line accuracy
//! from the diff hunk headers and structural immunity to the empty-grep
//! pipefail failure class that bit PRs #129/#130/#135.

use std::io::{self, BufWriter, Read, Write};
use std::process::Command;

use coder_precommit::parse_added_pub_items;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Input {
    diff: String,
    #[serde(default)]
    #[allow(dead_code)]
    workspace_root: String,
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
