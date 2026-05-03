//! planner-runner — full role lifecycle for planner-claude-agentry.
//!
//! EPIC #161 Wave 3 ports `PLANNER_CLAUDE_AGENTRY_SCRIPT` (the bash
//! heredoc that read `discovery.json`, asked claude to decompose the
//! meta-brief into a JSON array of child briefs, materialised each as a
//! brief JSON file under `/workspace/planner-children/`, then emitted a
//! `_chain_trigger` outbox message with absolute host paths) to a Rust
//! runner binary, mirroring the Wave 2 pattern (auditor-claude-runner)
//! and the in-flight Wave 3 archaeologist-runner. The role's
//! entrypoint_script becomes a one-line shell wrapper that execs
//! `/usr/local/bin/planner-runner`.
//!
//! ## Phases (verbatim port of bash semantics)
//!
//! 1. Read startup bundle on stdin, extract brief id / payload defaults.
//! 2. `discovery.json` must exist at `/workspace/discovery.json` —
//!    upstream archaeologist is responsible for producing it.
//! 3. `mkdir -p /workspace/planner-children`.
//! 4. Bound the inline discovery slice in the prompt to ~50 KiB
//!    (`DISCOVERY_PROMPT_LIMIT`), tagging the prompt with truncated=true
//!    when the cut fired.
//! 5. Build the planner prompt and stream `claude -p` to
//!    `/transcripts/<brief_id>.planner.jsonl`.
//! 6. Strip fences, slice the outer `[…]`, parse as a JSON array. Cap
//!    at `max_children` entries.
//! 7. For each element, write
//!    `/workspace/planner-children/child-<i>.json` and accumulate the
//!    absolute host path on `refs[]`.
//! 8. Emit one `_chain_trigger` outbox `Message` with
//!    `next_brief_refs: refs` so the daemon's terminal handler dispatches
//!    each child brief when this brief ships. The sentinel target name
//!    matches the bash — there is no role of that name in the planner
//!    topology, the daemon scans every accumulated outbox payload for
//!    `next_brief_refs` regardless of `to`.
//! 9. `emit_done shipped` — `DoneGuard` covers any unwound path so the
//!    daemon always sees a terminal `done` event (EPIC #161 B0 invariant).

use std::path::Path;

use agentry_role_runtime::planner::{
    build_child_brief_now, build_planner_prompt, cap_children, discovery_excerpt,
    parse_planner_payload, parse_planner_response, PlannerPayload,
};
use agentry_role_runtime::{
    emit_done, emit_event, emit_message, head_bytes, read_bundle_value, stream_claude, DoneGuard,
    StreamErr,
};
use orchestrator_types::{DoneReason, EventVerdict};
use serde_json::{json, Value};

const DISCOVERY_PATH: &str = "/workspace/discovery.json";
const CHILDREN_DIR: &str = "/workspace/planner-children";
const HOST_WORKSPACE_PREFIX: &str = "/var/mnt/workspaces/agentry-work/briefs";
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
                }),
            );
            return;
        }
    };

    let payload = parse_planner_payload(&bundle);

    if !Path::new(DISCOVERY_PATH).is_file() {
        emit_event(json!({
            "error": "discovery.json missing — upstream archaeologist must produce it at /workspace/discovery.json",
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    if let Err(e) = std::fs::create_dir_all(CHILDREN_DIR) {
        emit_event(json!({
            "error": "failed to create planner-children dir",
            "detail": e.to_string(),
        }));
        emit_done(EventVerdict::Failed, None);
        return;
    }

    let discovery_text = match std::fs::read_to_string(DISCOVERY_PATH) {
        Ok(s) => s,
        Err(e) => {
            emit_event(json!({
                "error": "failed to read discovery.json",
                "detail": e.to_string(),
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    let (excerpt, truncated, original_size) = discovery_excerpt(&discovery_text);

    let prompt = build_planner_prompt(
        &payload.intent,
        &payload.success_criteria,
        original_size,
        truncated,
        &excerpt,
        &payload.target_repo,
        &payload.base_branch,
        payload.max_children,
    );
    emit_event(json!({
        "msg": "calling claude -p",
        "prompt_bytes": prompt.len(),
    }));

    let response = match stream_claude(&payload.brief_id, ".planner", &prompt) {
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
                }),
            );
            return;
        }
    };

    let elements = match parse_planner_response(&response) {
        Some(arr) => arr,
        None => {
            emit_event(json!({
                "error": "claude response missing or malformed JSON array",
                "head": head_bytes(&response, RESPONSE_ERR_HEAD),
            }));
            emit_done(EventVerdict::Failed, None);
            return;
        }
    };

    let elements = cap_children(elements, payload.max_children);
    let count = elements.len();

    let host_workspace = format!("{HOST_WORKSPACE_PREFIX}/{}", payload.brief_id);
    let refs = write_children(&payload, &elements, &host_workspace);

    emit_message("_chain_trigger", json!({"next_brief_refs": refs}));
    emit_event(json!({
        "msg": "planner produced N children",
        "count": count,
        "manifest": format!("{CHILDREN_DIR}/"),
    }));
    emit_done(EventVerdict::Shipped, None);
}

/// Write each element as a child brief JSON file and accumulate absolute
/// host paths matching the daemon's chain-trigger expectations. Failed
/// writes are surfaced as warning events but do not abort the run — the
/// other children may still dispatch successfully (mirrors the bash
/// best-effort `> "$child_path"` behaviour).
fn write_children(
    payload: &PlannerPayload,
    elements: &[Value],
    host_workspace: &str,
) -> Vec<String> {
    let mut refs: Vec<String> = Vec::with_capacity(elements.len());
    for (i, elem) in elements.iter().enumerate() {
        let child = build_child_brief_now(
            &payload.brief_id,
            i,
            elem,
            &payload.child_topology,
            &payload.target_repo,
            &payload.base_branch,
        );
        let filename = format!("child-{i}.json");
        let full = format!("{CHILDREN_DIR}/{filename}");
        match std::fs::write(&full, child.to_string()) {
            Ok(_) => refs.push(format!("{host_workspace}/planner-children/{filename}")),
            Err(e) => {
                emit_event(json!({
                    "warn": "failed to write child brief",
                    "index": i,
                    "detail": e.to_string(),
                }));
            }
        }
    }
    refs
}
