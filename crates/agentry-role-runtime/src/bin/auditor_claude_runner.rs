//! auditor-claude-runner — full role lifecycle for auditor-claude-agentry.
//!
//! EPIC #161 Wave 2 ports `AUDITOR_CLAUDE_AGENTRY_SCRIPT` (~180 LoC bash
//! heredoc) to a Rust runner binary, mirroring the Wave 1 pattern
//! (coder-claude-runner, reviewer-claude-runner, ac-verifier-runner,
//! null-agent). The role's entrypoint_script becomes a one-line shell
//! wrapper that execs `/usr/local/bin/auditor-claude-runner`.
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! 1. Read startup bundle on stdin, extract `brief.id`.
//! 2. `cd /workspace`. Failure → emit `cd /workspace failed` + done failed.
//! 3. Emit `auditor starting`.
//! 4. Cargo report stages (each tail-truncated to 8 KiB, soft-fail):
//!    - `cargo clippy --workspace --all-targets -- -D warnings` → `clippy_report`
//!    - `RUSTFLAGS=-Dwarnings cargo build --workspace` → `build_report`
//!    - `cargo test --workspace` → `test_report`
//!    - `cargo +nightly udeps --workspace --output json` → `udeps_report`
//!      (4 KiB tail; default to `{}` on failure / not-installed)
//! 5. ra-query unwraps stage — walk `crates/**/*.rs` (excluding tests),
//!    invoke `ra-query unwraps <file> --severity critical --format json`,
//!    aggregate per-file critical counts and the raw result. Emit
//!    `unwraps_report` with the aggregated tail. Skip cleanly with
//!    `ra_query_unavailable` when binary missing.
//! 6. ra-query complexity stage — same walk, `ra-query complexity <file>
//!    --threshold 15 --format json`. Emit `complexity_report` /
//!    `ra_query_unavailable_complexity`.
//! 7. ra-query pub-surface + callers stage — walk crates' Cargo.toml at
//!    `crates/*/Cargo.toml`, run `ra-query pub-surface <crate_dir>` then
//!    `ra-query callers <file>:<line>` per item; per-file dead-pub items
//!    (zero callers, lib.rs excluded). Emit `pub_surface_report` /
//!    `ra_query_unavailable_pub_surface`.
//! 8. Self-heal child-brief dispatch — for each top-3 unwrap file, top-3
//!    dead-pub file, and every udep pair, write a child brief JSON to
//!    `/workspace/audit-children/`. Track host-mapped paths in `refs[]`.
//! 9. If `refs[]` non-empty, emit a `_chain_trigger` message with
//!    `next_brief_refs:[...]` so the daemon's terminal handler dispatches
//!    the children when the auditor ships.
//! 10. emit_done shipped.
//!
//! Every stage tolerates its underlying tool being absent — the bash used
//! `command -v` guards and `|| echo` fallbacks; the Rust port mirrors with
//! `Command::spawn` NotFound branches and `_unavailable` events.
//!
//! `DoneGuard` covers any unwound path (panic, abrupt return) so the
//! daemon always sees a terminal `done` event (EPIC #161 B0 invariant).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use agentry_role_runtime::{
    emit_done, emit_event, emit_finding, emit_message, orphan_pub_finding, parse_callers_count,
    parse_diff_added_lines, parse_unwraps_findings, pointer_str, pointer_str_or,
    ra_query_skipped_event, read_bundle_value, tail_bytes, DoneGuard,
};
use chrono::Utc;
use orchestrator_types::{DoneReason, EventVerdict};
use serde_json::{json, Value};

const WORKSPACE_DIR: &str = "/workspace";
const HOST_WORKSPACE_PREFIX: &str = "/var/mnt/workspaces/agentry-work/briefs";
const CARGO_TAIL: usize = 8192;
const UDEPS_TAIL: usize = 4096;
const FINDINGS_TAIL: usize = 8192;
const TOP_N_FILES: usize = 3;

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
                }),
            );
            return;
        }
    };

    let brief_id = pointer_str(&bundle, "/brief/id").to_string();

    if std::env::set_current_dir(WORKSPACE_DIR).is_err() {
        emit_event(json!({"error": "cd /workspace failed"}));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    emit_event(json!({"msg": "auditor starting"}));

    // ----- cargo report stages (soft-fail, tail-truncated) -----
    let clippy_out = run_capturing(
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        &[],
    );
    emit_event(json!({"msg": "clippy_report", "out": tail_bytes(&clippy_out, CARGO_TAIL)}));

    let build_out = run_capturing(
        "cargo",
        &["build", "--workspace"],
        &[("RUSTFLAGS", "-Dwarnings")],
    );
    emit_event(json!({"msg": "build_report", "out": tail_bytes(&build_out, CARGO_TAIL)}));

    let test_out = run_capturing("cargo", &["test", "--workspace"], &[]);
    emit_event(json!({"msg": "test_report", "out": tail_bytes(&test_out, CARGO_TAIL)}));

    let udeps_json_text = run_udeps_json();
    emit_event(
        json!({"msg": "udeps_report", "out": tail_bytes(udeps_json_text.as_bytes(), UDEPS_TAIL)}),
    );
    let udeps_value: Value = serde_json::from_str(&udeps_json_text).unwrap_or_else(|_| json!({}));

    // ----- ra-query unwraps stage -----
    let mut unwrap_findings: Vec<Value> = Vec::new();
    if which_on_path("ra-query") {
        let mut total_critical: u64 = 0;
        for file in walk_crate_rs_files() {
            let res = ra_query_capture(&[
                "unwraps",
                file.to_string_lossy().as_ref(),
                "--severity",
                "critical",
                "--format",
                "json",
            ])
            .unwrap_or_else(|| json!({"functions": []}));
            let critical_count = sum_unwraps_count(&res);
            if critical_count > 0 {
                unwrap_findings.push(json!({
                    "file": file.to_string_lossy(),
                    "critical_count": critical_count,
                    "result": res,
                }));
                total_critical += critical_count;
            }
        }
        let findings_json_tail = tail_bytes(
            serde_json::to_string(&unwrap_findings)
                .unwrap_or_else(|_| "[]".into())
                .as_bytes(),
            FINDINGS_TAIL,
        );
        emit_event(json!({
            "msg": "unwraps_report",
            "critical_total": total_critical,
            "findings_json_tail": findings_json_tail,
        }));
    } else {
        emit_event(json!({
            "msg": "ra_query_unavailable",
            "detail": "skipping unwraps stage",
        }));
    }

    // ----- ra-query complexity stage -----
    if which_on_path("ra-query") {
        let mut cfindings: Vec<Value> = Vec::new();
        let mut total_complex: u64 = 0;
        for file in walk_crate_rs_files() {
            let res = ra_query_capture(&[
                "complexity",
                file.to_string_lossy().as_ref(),
                "--threshold",
                "15",
                "--format",
                "json",
            ])
            .unwrap_or_else(|| json!({"functions": []}));
            let complex_count = res
                .get("functions")
                .and_then(Value::as_array)
                .map(|a| a.len() as u64)
                .unwrap_or(0);
            if complex_count > 0 {
                cfindings.push(json!({
                    "file": file.to_string_lossy(),
                    "complex_count": complex_count,
                    "result": res,
                }));
                total_complex += complex_count;
            }
        }
        let findings_json_tail = tail_bytes(
            serde_json::to_string(&cfindings)
                .unwrap_or_else(|_| "[]".into())
                .as_bytes(),
            FINDINGS_TAIL,
        );
        emit_event(json!({
            "msg": "complexity_report",
            "complex_total": total_complex,
            "findings_json_tail": findings_json_tail,
        }));
    } else {
        emit_event(json!({
            "msg": "ra_query_unavailable_complexity",
            "detail": "skipping complexity stage",
        }));
    }

    // ----- ra-query pub-surface + callers stage -----
    let mut pub_findings: Vec<Value> = Vec::new();
    if which_on_path("ra-query") {
        let mut total_dead_pub: u64 = 0;
        for ctoml in walk_crate_cargo_tomls() {
            let cdir = match ctoml.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };
            let pout = ra_query_capture(&[
                "pub-surface",
                cdir.to_string_lossy().as_ref(),
                "--format",
                "json",
            ])
            .unwrap_or_else(|| json!([]));
            let items = match pout.as_array() {
                Some(a) => a.clone(),
                None => continue,
            };
            // Group dead items per file.
            let mut crate_dead: std::collections::BTreeMap<String, Vec<Value>> =
                std::collections::BTreeMap::new();
            for item in &items {
                let ifile = item
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if ifile.is_empty() {
                    continue;
                }
                if ifile.ends_with("/lib.rs") {
                    continue;
                }
                let iline = item.get("line").and_then(Value::as_u64).unwrap_or(0);
                let iname = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let pos = format!("{ifile}:{iline}");
                let cout = ra_query_capture(&["callers", &pos, "--format", "json"])
                    .unwrap_or_else(|| json!({"callers": []}));
                let ccnt = cout
                    .get("callers")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                if ccnt == 0 {
                    crate_dead
                        .entry(ifile)
                        .or_default()
                        .push(json!({"name": iname, "line": iline}));
                }
            }
            for (file, dead_items) in crate_dead {
                let dead_count = dead_items.len() as u64;
                pub_findings.push(json!({
                    "file": file,
                    "dead_count": dead_count,
                    "items": dead_items,
                }));
                total_dead_pub += dead_count;
            }
        }
        let findings_json_tail = tail_bytes(
            serde_json::to_string(&pub_findings)
                .unwrap_or_else(|_| "[]".into())
                .as_bytes(),
            FINDINGS_TAIL,
        );
        emit_event(json!({
            "msg": "pub_surface_report",
            "dead_pub_total": total_dead_pub,
            "findings_json_tail": findings_json_tail,
        }));
    } else {
        emit_event(json!({
            "msg": "ra_query_unavailable_pub_surface",
            "detail": "skipping pub-surface stage",
        }));
    }

    // ----- ra-query unwraps stage (per-site Warn findings, brief #87 phase2) -----
    //
    // Distinct from the aggregate `unwraps_report` above: that emits one
    // event with file-level critical counts so the self-heal dispatch can
    // rank top-3 files; this stage emits one Warn ReviewFinding per
    // unwrap site so downstream dashboards / coder rework prompts can
    // address sites individually. Severity is always Warn — the brief
    // calls out NEVER emitting Blocker.
    'ra_unwraps_findings: {
        if !which_on_path("ra-query") {
            emit_event(ra_query_skipped_event(
                "unwraps_findings",
                "ra-query binary missing",
            ));
            break 'ra_unwraps_findings;
        }
        let mut findings_emitted: u64 = 0;
        for file in walk_crate_rs_files() {
            let file_str = file.to_string_lossy().to_string();
            let res = match ra_query_capture(&["unwraps", &file_str, "--format", "json"]) {
                Some(v) => v,
                None => continue,
            };
            for f in parse_unwraps_findings(&file_str, &res) {
                emit_finding(&f);
                findings_emitted += 1;
            }
        }
        emit_event(json!({
            "msg": "ra_query_unwraps_findings_complete",
            "findings_emitted": findings_emitted,
        }));
    }

    // ----- ra-query callers + pub-surface stage: orphan-pub Warn findings -----
    //
    // For each pub item INTRODUCED in this brief (line number falls
    // inside the unified-zero diff vs base_branch) and reachable by zero
    // workspace callers, emit a Warn `orphan_pub` finding. Best-effort:
    // a missing ra-query, git failure, or unparseable pub-surface output
    // shorts out with a `_skipped` event and the auditor proceeds.
    'ra_orphan_pub: {
        if !which_on_path("ra-query") {
            emit_event(ra_query_skipped_event(
                "orphan_pub",
                "ra-query binary missing",
            ));
            break 'ra_orphan_pub;
        }
        let base_branch = pointer_str_or(&bundle, "/brief/payload/base_branch", "develop");
        let diff_text = match git_diff_unified_zero(&base_branch) {
            Ok(d) => d,
            Err(e) => {
                emit_event(ra_query_skipped_event(
                    "orphan_pub",
                    &format!("git diff failed: {e}"),
                ));
                break 'ra_orphan_pub;
            }
        };
        let added: std::collections::BTreeMap<String, std::collections::BTreeSet<u32>> =
            parse_diff_added_lines(&diff_text);
        if added.is_empty() {
            emit_event(json!({
                "msg": "ra_query_orphan_pub_complete",
                "findings_emitted": 0,
                "detail": "no added lines in diff",
            }));
            break 'ra_orphan_pub;
        }
        let mut findings_emitted: u64 = 0;
        for ctoml in walk_crate_cargo_tomls() {
            let cdir = match ctoml.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };
            let pout = match ra_query_capture(&[
                "pub-surface",
                cdir.to_string_lossy().as_ref(),
                "--format",
                "json",
            ]) {
                Some(v) => v,
                None => continue,
            };
            let items = match pout.as_array() {
                Some(a) => a.clone(),
                None => continue,
            };
            for item in items {
                let ifile = match item.get("file").and_then(Value::as_str) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => continue,
                };
                // pub-surface emits absolute paths; the diff map is
                // workspace-relative. Normalise both ends.
                let ifile_rel = ifile
                    .strip_prefix(&format!("{WORKSPACE_DIR}/"))
                    .map(str::to_string)
                    .unwrap_or_else(|| ifile.clone());
                let iline = match item.get("line").and_then(Value::as_u64) {
                    Some(l) => l as u32,
                    None => continue,
                };
                let added_lines = match added.get(&ifile_rel) {
                    Some(s) => s,
                    None => continue,
                };
                if !added_lines.contains(&iline) {
                    continue;
                }
                let iname = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let ikind = item
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("item")
                    .to_string();
                let pos = format!("{ifile_rel}:{iline}:1");
                let cout = match ra_query_capture(&["callers", &pos, "--format", "json"]) {
                    Some(v) => v,
                    None => continue,
                };
                if parse_callers_count(&cout) == 0 {
                    emit_finding(&orphan_pub_finding(&ifile_rel, iline, &iname, &ikind));
                    findings_emitted += 1;
                }
            }
        }
        emit_event(json!({
            "msg": "ra_query_orphan_pub_complete",
            "findings_emitted": findings_emitted,
        }));
    }

    // ----- Self-heal child-brief dispatch -----
    let _ = std::fs::create_dir_all(format!("{WORKSPACE_DIR}/audit-children"));
    let host_workspace = format!("{HOST_WORKSPACE_PREFIX}/{brief_id}");
    let mut refs: Vec<String> = Vec::new();

    // Top 3 unwrap files: rank by critical_count desc.
    let mut top_unwraps: Vec<&Value> = unwrap_findings.iter().collect();
    top_unwraps.sort_by_key(|v| {
        std::cmp::Reverse(v.get("critical_count").and_then(Value::as_u64).unwrap_or(0))
    });
    for (j, finding) in top_unwraps.into_iter().take(TOP_N_FILES).enumerate() {
        let ufile = finding.get("file").and_then(Value::as_str).unwrap_or("");
        let base = basename(ufile);
        let sites = build_unwrap_sites_block(finding);
        let child = build_unwrap_child_brief(&brief_id, j, ufile, &base, &sites);
        let path = write_child(j, "unwrap", &child);
        if let Some(p) = path {
            refs.push(format!("{host_workspace}/audit-children/{p}"));
        }
    }

    // Top 3 dead-pub files: rank by dead_count desc.
    let mut top_dead: Vec<&Value> = pub_findings.iter().collect();
    top_dead.sort_by_key(|v| {
        std::cmp::Reverse(v.get("dead_count").and_then(Value::as_u64).unwrap_or(0))
    });
    for (j, finding) in top_dead.into_iter().take(TOP_N_FILES).enumerate() {
        let pfile = finding.get("file").and_then(Value::as_str).unwrap_or("");
        let base = basename(pfile);
        let sites = build_dead_pub_sites_block(finding);
        let child = build_dead_pub_child_brief(&brief_id, j, pfile, &base, &sites);
        let path = write_child(j, "dead-pub", &child);
        if let Some(p) = path {
            refs.push(format!("{host_workspace}/audit-children/{p}"));
        }
    }

    // udep pairs: one child brief per (crate, dep) pair.
    let udep_pairs = collect_udep_pairs(&udeps_value);
    for (i, (crate_name, dep)) in udep_pairs.iter().enumerate() {
        let child = build_udep_child_brief(&brief_id, i, crate_name, dep);
        let filename = format!("child-{i}.json");
        let full = format!("{WORKSPACE_DIR}/audit-children/{filename}");
        if std::fs::write(&full, child.to_string()).is_ok() {
            refs.push(format!("{host_workspace}/audit-children/{filename}"));
        }
    }

    if !refs.is_empty() {
        emit_message("_chain_trigger", json!({"next_brief_refs": refs}));
    }

    emit_done(EventVerdict::Shipped, None);
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

/// True iff `name --version` succeeds. Mirrors the bash `command -v <bin>` guard.
fn which_on_path(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command, capturing combined stdout+stderr. Mirrors bash
/// `cmd 2>&1 | tail -c N || true` — a missing binary or non-zero exit
/// leaves an empty buffer (or the spawn-error string), never panics.
fn run_capturing(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Vec<u8> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => return format!("spawn {program}: {e}").into_bytes(),
    };
    let mut combined = out.stdout;
    combined.extend_from_slice(&out.stderr);
    combined
}

/// `cargo +nightly udeps --workspace --output json` with bash semantics:
/// missing toolchain or non-zero exit → empty `{}` object string.
/// stderr is intentionally discarded (bash redirects it to /dev/null).
fn run_udeps_json() -> String {
    let out = match Command::new("cargo")
        .args(["+nightly", "udeps", "--workspace", "--output", "json"])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return "{}".to_string(),
    };
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    if s.trim().is_empty() {
        "{}".to_string()
    } else {
        s
    }
}

/// `git diff --unified=0 <base_branch>...HEAD` capturing stdout. Used by
/// the orphan-pub stage to identify pub items added in this brief. The
/// 3-dot range is symmetric (HEAD vs merge-base with base) — the same
/// shape `reviewer_claude_runner` uses for its review prompt.
fn git_diff_unified_zero(base_branch: &str) -> Result<String, String> {
    let range = format!("{base_branch}...HEAD");
    let out = Command::new("git")
        .args(["diff", "--unified=0", &range])
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git diff: {e}"))?;
    if !out.status.success() {
        let mut combined = Vec::with_capacity(out.stdout.len() + out.stderr.len());
        combined.extend_from_slice(&out.stdout);
        combined.extend_from_slice(&out.stderr);
        let tail = String::from_utf8_lossy(&combined).into_owned();
        let trimmed: String = tail.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
        return Err(trimmed);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Invoke `ra-query` and parse stdout as JSON. Returns `None` on spawn /
/// non-zero exit / unparseable output — mirrors bash `... 2>/dev/null ||
/// echo '<default>'` where the caller then fills in a sane default.
fn ra_query_capture(args: &[&str]) -> Option<Value> {
    let out = Command::new("ra-query")
        .args(args)
        .current_dir(WORKSPACE_DIR)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

// ---------------------------------------------------------------------------
// Filesystem walks
// ---------------------------------------------------------------------------

/// Walk `crates/` for `*.rs` files, excluding `tests/`, `tests.rs`, and
/// `target/`. Mirrors bash `find crates -name '*.rs' -not -path '*/tests/*'
/// -not -name 'tests.rs' -not -path '*/target/*'`.
fn walk_crate_rs_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(Path::new("crates"), &mut out, &mut |path| {
        if path.extension().and_then(OsStr::to_str) != Some("rs") {
            return false;
        }
        if path.file_name().and_then(OsStr::to_str) == Some("tests.rs") {
            return false;
        }
        let s = path.to_string_lossy();
        if s.contains("/tests/") || s.contains("/target/") {
            return false;
        }
        true
    });
    out
}

/// Walk `crates/*/Cargo.toml` (mindepth 2, maxdepth 2) excluding paths
/// under `target/`. Mirrors bash `find crates -mindepth 2 -maxdepth 2
/// -name 'Cargo.toml' -not -path '*/target/*'`.
fn walk_crate_cargo_tomls() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let crates_dir = Path::new("crates");
    let entries = match std::fs::read_dir(crates_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.to_string_lossy().contains("/target/") {
            continue;
        }
        let cargo = path.join("Cargo.toml");
        if cargo.is_file() {
            out.push(cargo);
        }
    }
    out
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>, accept: &mut dyn FnMut(&Path) -> bool) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            // Prune target/ early to avoid descending into compiled artifacts.
            if path.file_name().and_then(OsStr::to_str) == Some("target") {
                continue;
            }
            walk_dir(&path, out, accept);
        } else if ft.is_file() && accept(&path) {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// JSON aggregation helpers
// ---------------------------------------------------------------------------

/// Sum `[.functions[]?.unwraps[]?] | length` from an `ra-query unwraps`
/// JSON response. The bash uses jq for the same expression.
fn sum_unwraps_count(v: &Value) -> u64 {
    let functions = match v.get("functions").and_then(Value::as_array) {
        Some(a) => a,
        None => return 0,
    };
    let mut total = 0;
    for fn_v in functions {
        if let Some(unwraps) = fn_v.get("unwraps").and_then(Value::as_array) {
            total += unwraps.len() as u64;
        }
    }
    total
}

fn basename(s: &str) -> String {
    Path::new(s)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(s)
        .to_string()
}

fn build_unwrap_sites_block(finding: &Value) -> String {
    let functions = finding
        .pointer("/result/functions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut lines = Vec::new();
    for fn_v in functions {
        let fn_name = fn_v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let unwraps = fn_v
            .get("unwraps")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for u in unwraps {
            let file = u.get("file").and_then(Value::as_str).unwrap_or("?");
            let line = u.get("line").and_then(Value::as_u64).unwrap_or(0);
            let reason = u
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason");
            lines.push(format!(
                "  - {fn_name} at {file}:{line} — critical — {reason}"
            ));
        }
    }
    lines.join("\n")
}

fn build_dead_pub_sites_block(finding: &Value) -> String {
    let pfile = finding.get("file").and_then(Value::as_str).unwrap_or("?");
    let items = finding
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut lines = Vec::new();
    for item in items {
        let line = item.get("line").and_then(Value::as_u64).unwrap_or(0);
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        lines.push(format!("  - {pfile}:{line} — {name}"));
    }
    lines.join("\n")
}

fn build_unwrap_child_brief(
    brief_id: &str,
    j: usize,
    ufile: &str,
    base: &str,
    sites: &str,
) -> Value {
    json!({
        "id": format!("brf_self_heal_{brief_id}_unwrap_{j}"),
        "project": Value::Null,
        "topology": {"name": "agentry-self-host-v0", "version": 1},
        "payload": {
            "issue_number": 0,
            "issue_title": format!("fix(unwraps): replace critical unwraps in {base}"),
            "issue_body": format!(
                "Replace critical unwraps in {ufile}.\n\nSites:\n{sites}\n\nFor each site choose the right replacement: ? if the function returns Result/Option, expect(\"<context>\") if the invariant truly holds and you can articulate why, unwrap_or / unwrap_or_else / ok_or if a fallback is appropriate. Do NOT silently swallow errors. Do NOT add bare expect(\"\") or expect(\"this should not fail\") — provide real context."
            ),
            "acceptance": "cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && scripts/arch-check.sh",
            "target_repo": "yg/agentry",
            "base_branch": "develop",
            "pr_title": format!("fix(unwraps): replace critical unwraps in {base}"),
            "pr_body": format!(
                "Auto-dispatched by auditor (ra-query unwraps --severity critical, file ranked top-{j} by critical count)."
            ),
        },
        "budget": {"max_wall_seconds": 1500},
        "escalation": "autonomous",
        "parent_brief": brief_id,
        "submitted_by": "auditor-self-heal",
        "submitted_at": Utc::now().to_rfc3339(),
    })
}

fn build_dead_pub_child_brief(
    brief_id: &str,
    j: usize,
    pfile: &str,
    base: &str,
    sites: &str,
) -> Value {
    json!({
        "id": format!("brf_self_heal_{brief_id}_dead_pub_{j}"),
        "project": Value::Null,
        "topology": {"name": "agentry-self-host-v0", "version": 1},
        "payload": {
            "issue_number": 0,
            "issue_title": format!("fix(dead-pub): remove or expose dead pub items in {base}"),
            "issue_body": format!(
                "Dead pub items in {pfile} (zero workspace callers per ra-query callers).\n\nSites:\n{sites}\n\nFor each site: DELETE the pub keyword OR add a `pub use` re-export in lib.rs to expose it as documented API surface. Do NOT silently leave items pub-but-unused — pick one path and apply it."
            ),
            "acceptance": "cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && scripts/arch-check.sh",
            "target_repo": "yg/agentry",
            "base_branch": "develop",
            "pr_title": format!("fix(dead-pub): remove or expose dead pub items in {base}"),
            "pr_body": format!(
                "Auto-dispatched by auditor (ra-query pub-surface + ra-query callers, file ranked top-{j} by dead-pub count)."
            ),
        },
        "budget": {"max_wall_seconds": 1500},
        "escalation": "autonomous",
        "parent_brief": brief_id,
        "submitted_by": "auditor-self-heal",
        "submitted_at": Utc::now().to_rfc3339(),
    })
}

fn build_udep_child_brief(brief_id: &str, i: usize, crate_name: &str, dep: &str) -> Value {
    json!({
        "id": format!("brf_self_heal_{brief_id}_udep_{i}"),
        "project": Value::Null,
        "topology": {"name": "agentry-bugfix-v0", "version": 1},
        "payload": {
            "issue_number": 0,
            "issue_title": format!("fix(deps): remove unused {dep} from {crate_name}"),
            "issue_body": format!(
                "DELETE `{dep}.workspace = true` from crates/{crate_name}/Cargo.toml. cargo-udeps reports unused."
            ),
            "acceptance": "cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && RUSTFLAGS=\"-Dwarnings\" cargo build --workspace && cargo test --workspace",
            "target_repo": "yg/agentry",
            "base_branch": "develop",
            "pr_title": format!("fix(deps): remove unused {dep} from {crate_name}"),
            "pr_body": "Auto-dispatched by auditor.",
        },
        "budget": {"max_wall_seconds": 900},
        "escalation": "autonomous",
        "parent_brief": brief_id,
        "submitted_by": "auditor-self-heal",
        "submitted_at": Utc::now().to_rfc3339(),
    })
}

fn write_child(j: usize, kind: &str, child: &Value) -> Option<String> {
    let filename = format!("child-{kind}-{j}.json");
    let full = format!("{WORKSPACE_DIR}/audit-children/{filename}");
    match std::fs::write(&full, child.to_string()) {
        Ok(_) => Some(filename),
        Err(e) => {
            emit_event(json!({
                "warn": "failed to write child brief",
                "kind": kind,
                "index": j,
                "detail": e.to_string(),
            }));
            None
        }
    }
}

/// Mirror the bash jq pipeline:
/// `[.unused_deps // {} | to_entries[] | .key as $k |
///  ((.value.normal // []) + (.value.development // []) + (.value.build // []))[]
///   as $d | {crate:($k|split(" ")[0]), dep:$d}]`
fn collect_udep_pairs(udeps: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let unused = match udeps.get("unused_deps").and_then(Value::as_object) {
        Some(o) => o,
        None => return out,
    };
    for (key, value) in unused {
        // `$k|split(" ")[0]` — first whitespace-separated token.
        let crate_name = key.split_whitespace().next().unwrap_or(key).to_string();
        for kind in &["normal", "development", "build"] {
            let arr = value
                .get(*kind)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for dep in arr {
                if let Some(s) = dep.as_str() {
                    out.push((crate_name.clone(), s.to_string()));
                }
            }
        }
    }
    out
}
