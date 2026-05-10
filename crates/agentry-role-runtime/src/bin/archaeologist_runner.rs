//! archaeologist-claude-runner — full role lifecycle for
//! archaeologist-claude-agentry.
//!
//! EPIC #161 Wave 3 ports `ARCHAEOLOGIST_CLAUDE_AGENTRY_SCRIPT` (the
//! `cfdb extract` + `graph-specs check` + seed-query + claude → JSON
//! object → `discovery.json` heredoc) to a Rust runner binary, mirroring
//! the Wave 1/2 pattern (auditor-claude-runner, reviewer-claude-runner,
//! coder-claude-runner). The role's entrypoint_script becomes a one-line
//! shell wrapper that execs `/usr/local/bin/archaeologist-runner`.
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! 1. Read startup bundle on stdin, extract `brief.id`,
//!    `brief.payload.{intent,success_criteria,discovery_seeds}`.
//! 2. Workspace must be a git repo — missing `.git` → `done failed`.
//! 3. `cd /workspace`. Failure → `done failed`.
//! 4. Run `cfdb extract --workspace . --db .cfdb/db-discovery
//!    --keyspace agentry`; on non-zero exit emit the last 30 lines of
//!    combined output and `done failed`. Parse `extract: N nodes, M
//!    edges` and emit `cfdb extract done`.
//! 5. Run `graph-specs check --specs specs/concepts/ --code crates/
//!    --json` (soft-fail). Capture combined stdout+stderr, emit a 500-byte
//!    head as `graph-specs done`.
//! 6. For each seed query in the bundle, run
//!    `cfdb query --db .cfdb/db-discovery --keyspace agentry "$q"`.
//!    Failure or non-JSON output → `[]` for that query's `rows`. Build a
//!    JSON array of `{query, rows}` objects.
//! 7. Build the archaeologist prompt and stream `claude -p` to
//!    `/transcripts/<brief_id>.archaeologist.jsonl`.
//! 8. Strip fences, slice the outer `{…}`, parse as a JSON object, write
//!    to `/workspace/discovery.json`. Emit byte count.
//! 9. `emit_done shipped` — `DoneGuard` covers any unwound path so the
//!    daemon always sees a terminal `done` event (EPIC #161 B0 invariant).

use std::process::{Command, Stdio};

use agentry_role_runtime::archaeologist::{
    build_archaeologist_prompt, parse_cfdb_counts, parse_discovery_object, parse_discovery_seeds,
};
use agentry_role_runtime::{
    emit_done, emit_event, head_bytes, pointer_str, read_bundle_value, stream_claude, tail_lines,
    workspace_is_git_repo, DoneGuard, StreamErr,
};
use orchestrator_types::{DoneReason, EventVerdict};
use serde_json::{json, Value};

const WORKSPACE_DIR: &str = "/workspace";
const CFDB_EXTRACT_TAIL_LINES: usize = 30;
const GRAPH_SPECS_EVENT_HEAD: usize = 500;
const RESPONSE_ERR_HEAD: usize = 300;

fn main() {
    let _guard = DoneGuard::new();

    let bundle = match read_bundle_value() {
        Ok(v) => v,
        Err(e) => {
            emit_event(json!({
                "error": "failed to parse startup bundle",
                "detail": e.to_string(),
            }));
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause: "bundle_parse_failed".into(),
                    exit_code: None,
                    disagreements: Vec::new(),
                }),
            );
            return;
        }
    };

    let brief_id = pointer_str(&bundle, "/brief/id").to_string();
    let intent = pointer_str(&bundle, "/brief/payload/intent").to_string();
    let success_criteria = pointer_str(&bundle, "/brief/payload/success_criteria").to_string();
    let seeds = parse_discovery_seeds(&bundle);

    if !workspace_is_git_repo(WORKSPACE_DIR) {
        emit_event(json!({
            "error": "workspace missing — no .git found at /workspace",
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    if std::env::set_current_dir(WORKSPACE_DIR).is_err() {
        emit_event(json!({"error": "cd /workspace failed"}));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    emit_event(json!({"msg": "running cfdb extract"}));
    let extract_log = match run_cfdb_extract() {
        Ok(log) => log,
        Err(detail) => {
            emit_event(json!({
                "error": "cfdb extract failed",
                "detail": detail,
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    let (nodes, edges) = parse_cfdb_counts(&extract_log);
    emit_event(json!({
        "msg": "cfdb extract done",
        "nodes": nodes,
        "edges": edges,
    }));

    let graph_specs_out = run_graph_specs();
    emit_event(json!({
        "msg": "graph-specs done",
        "head": head_bytes(&graph_specs_out, GRAPH_SPECS_EVENT_HEAD),
    }));

    let seed_results = run_seed_queries(&seeds);
    let seed_results_json = serde_json::to_string(&seed_results).unwrap_or_else(|_| "[]".into());

    let prompt = build_archaeologist_prompt(
        &intent,
        &success_criteria,
        nodes,
        edges,
        &graph_specs_out,
        &seed_results_json,
    );
    emit_event(json!({
        "msg": "calling claude -p",
        "prompt_bytes": prompt.len(),
    }));

    let response = match stream_claude(&brief_id, ".archaeologist", &prompt) {
        Ok(r) => r,
        Err(StreamErr::ClaudeFailed { exit_code, detail }) => {
            emit_event(json!({
                "error": "claude -p failed",
                "exit_code": exit_code,
                "detail": detail,
            }));
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause: "claude_failed".into(),
                    exit_code: Some(exit_code),
                    disagreements: Vec::new(),
                }),
            );
            return;
        }
        Err(StreamErr::TranscriptEmpty { path }) => {
            emit_event(json!({
                "error": "tee_or_transcript_write_failed",
                "transcript_path": path,
            }));
            emit_done(
                EventVerdict::Failed,
                Some(DoneReason {
                    cause: "transcript_empty".into(),
                    exit_code: None,
                    disagreements: Vec::new(),
                }),
            );
            return;
        }
    };

    let payload = match parse_discovery_object(&response) {
        Some(v) => v,
        None => {
            emit_event(json!({
                "error": "claude response missing or malformed JSON object",
                "head": head_bytes(&response, RESPONSE_ERR_HEAD),
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    let discovery_path = format!("{WORKSPACE_DIR}/discovery.json");
    let serialized = payload.to_string();
    if let Err(e) = std::fs::write(&discovery_path, &serialized) {
        emit_event(json!({
            "error": "failed to write discovery.json",
            "detail": e.to_string(),
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    emit_event(json!({
        "msg": "discovery.json written",
        "path": discovery_path,
        "bytes": serialized.len(),
    }));
    emit_done(EventVerdict::Shipped, None);
}

/// Run `cfdb extract` and capture combined stdout+stderr. On non-zero
/// exit, return the last 30 lines (matching bash `tail -30`) as the
/// error detail.
fn run_cfdb_extract() -> Result<String, String> {
    let out = Command::new("cfdb")
        .args([
            "extract",
            "--workspace",
            ".",
            "--db",
            ".cfdb/db-discovery",
            "--keyspace",
            "agentry",
        ])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn cfdb extract: {e}"))?;

    let mut combined = out.stdout;
    combined.extend_from_slice(&out.stderr);

    if !out.status.success() {
        return Err(tail_lines(&combined, CFDB_EXTRACT_TAIL_LINES));
    }
    Ok(String::from_utf8_lossy(&combined).into_owned())
}

/// Run `graph-specs check` and return combined stdout+stderr as a lossy
/// UTF-8 string. Soft-fail: a non-zero exit OR a missing binary still
/// returns a string (mirrors bash `2>&1 || true`).
fn run_graph_specs() -> String {
    match Command::new("graph-specs")
        .args([
            "check",
            "--specs",
            "specs/concepts/",
            "--code",
            "crates/",
            "--json",
        ])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => {
            let mut combined = o.stdout;
            combined.extend_from_slice(&o.stderr);
            String::from_utf8_lossy(&combined).into_owned()
        }
        Err(e) => format!("graph-specs spawn error: {e}"),
    }
}

/// Run each seed query against the just-built db. On failure or
/// non-JSON output, fall back to `[]` for that query's rows. Returns a
/// JSON array of `{query, rows}` objects matching the bash
/// `seed_results` accumulator.
fn run_seed_queries(seeds: &[String]) -> Value {
    let mut out: Vec<Value> = Vec::with_capacity(seeds.len());
    for query in seeds {
        let rows = run_cfdb_query(query);
        out.push(json!({"query": query, "rows": rows}));
    }
    Value::Array(out)
}

fn run_cfdb_query(query: &str) -> Value {
    let result = Command::new("cfdb")
        .args([
            "query",
            "--db",
            ".cfdb/db-discovery",
            "--keyspace",
            "agentry",
            query,
        ])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let stdout = match result {
        Ok(o) if o.status.success() => o.stdout,
        _ => return json!([]),
    };
    serde_json::from_slice::<Value>(&stdout).unwrap_or_else(|_| json!([]))
}
