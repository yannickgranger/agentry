//! Pure helpers for the planner-claude-runner binary (EPIC #161 Wave 3).
//! Lives in the lib crate so the test crate at
//! `crates/agentry-role-runtime/tests/planner_runner_test.rs` can reach
//! them without touching the `src/bin/` file (the
//! `arch-ban-inline-cfg-test-in-src` rule forbids inline `#[cfg(test)]`
//! modules under `src/`).

use chrono::Utc;
use serde_json::{json, Value};

use crate::{head_bytes, pointer_str, pointer_str_or, slice_json_array, strip_fences};

/// Bash `head -c 51200` — bound the inline discovery slice in the prompt
/// to ~50 KiB.
pub const DISCOVERY_PROMPT_LIMIT: usize = 51200;

/// Default cap on the number of child briefs the planner is allowed to
/// emit when the meta-brief payload omits `max_children`.
pub const DEFAULT_MAX_CHILDREN: u64 = 10;

/// Default `topology` applied to a child brief when the claude reply does
/// not pick one explicitly.
pub const DEFAULT_CHILD_TOPOLOGY: &str = "agentry-self-host-v0";

/// Parsed meta-brief payload as consumed by the planner runner. Defaults
/// mirror the bash `jq -r '... // "..."'` fall-through values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannerPayload {
    pub brief_id: String,
    pub intent: String,
    pub success_criteria: String,
    pub child_topology: String,
    pub max_children: u64,
    pub base_branch: String,
    pub target_repo: String,
}

/// Extract the planner inputs from a startup bundle JSON value.
///
/// Mirrors the bash:
/// ```sh
/// brief_id=$(jq -r '.brief.id' <<<"$bundle")
/// intent=$(jq -r '.brief.payload.intent // ""' <<<"$bundle")
/// success_criteria=$(jq -r '.brief.payload.success_criteria // ""' <<<"$bundle")
/// child_topology=$(jq -r '.brief.payload.child_topology // "agentry-self-host-v0"' <<<"$bundle")
/// max_children=$(jq -r '.brief.payload.max_children // 10' <<<"$bundle")
/// base_branch=$(jq -r '.brief.payload.base_branch // "develop"' <<<"$bundle")
/// target_repo=$(jq -r '.brief.payload.target_repo // "yg/agentry"' <<<"$bundle")
/// ```
pub fn parse_planner_payload(bundle: &Value) -> PlannerPayload {
    let brief_id = pointer_str(bundle, "/brief/id").to_string();
    let intent = pointer_str(bundle, "/brief/payload/intent").to_string();
    let success_criteria = pointer_str(bundle, "/brief/payload/success_criteria").to_string();
    let child_topology = pointer_str_or(
        bundle,
        "/brief/payload/child_topology",
        DEFAULT_CHILD_TOPOLOGY,
    );
    let max_children = bundle
        .pointer("/brief/payload/max_children")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_CHILDREN);
    let base_branch = pointer_str_or(bundle, "/brief/payload/base_branch", "develop");
    let target_repo = pointer_str_or(bundle, "/brief/payload/target_repo", "yg/agentry");
    PlannerPayload {
        brief_id,
        intent,
        success_criteria,
        child_topology,
        max_children,
        base_branch,
        target_repo,
    }
}

/// Slice the discovery file content into the prompt-budget excerpt.
/// Returns `(excerpt, truncated, original_size)`.
///
/// Mirrors bash:
/// ```sh
/// discovery_size=$(wc -c < /workspace/discovery.json)
/// if [ "$discovery_size" -gt 51200 ]; then
///     discovery_excerpt=$(head -c 51200 /workspace/discovery.json)
///     discovery_truncated="true"
/// else
///     discovery_excerpt=$(cat /workspace/discovery.json)
///     discovery_truncated="false"
/// fi
/// ```
///
/// The Rust port snaps the cut down to a UTF-8 char boundary so multi-byte
/// chars don't get split — bash cut on raw bytes; both produce the same
/// prompt for the ASCII discovery JSON the archaeologist emits today.
pub fn discovery_excerpt(content: &str) -> (String, bool, usize) {
    let original_size = content.len();
    if original_size > DISCOVERY_PROMPT_LIMIT {
        (
            head_bytes(content, DISCOVERY_PROMPT_LIMIT),
            true,
            original_size,
        )
    } else {
        (content.to_string(), false, original_size)
    }
}

/// Build the planner claude prompt — verbatim port of the bash
/// `cat > /tmp/planner_prompt.txt <<PROMPT` heredoc.
#[allow(clippy::too_many_arguments)]
pub fn build_planner_prompt(
    intent: &str,
    success_criteria: &str,
    discovery_size: usize,
    discovery_truncated: bool,
    discovery_excerpt: &str,
    target_repo: &str,
    base_branch: &str,
    max_children: u64,
) -> String {
    let truncated_str = if discovery_truncated { "true" } else { "false" };
    let mut s = String::new();
    s.push_str(
        "You are the planner role for the agentry project. Decompose the META-BRIEF\n\
         intent into a JSON ARRAY of child briefs. Each child must be a focused,\n\
         verifiable transformation expressed as verbs (CREATE/UPDATE/REPLACE/DELETE/MOVE)\n\
         on specific file:line targets — NOT freeform \"fix this issue\" prose.\n\
         \n",
    );
    s.push_str(&format!("META-BRIEF INTENT:\n{intent}\n\n"));
    s.push_str(&format!("SUCCESS CRITERIA:\n{success_criteria}\n\n"));
    s.push_str(&format!(
        "DISCOVERY (size={discovery_size} bytes, truncated={truncated_str}):\n{discovery_excerpt}\n\n"
    ));
    s.push_str(&format!(
        "CHILD BOILERPLATE (apply to every element):\n\
         - target_repo: {target_repo}\n\
         - base_branch: {base_branch}\n\
         - budget.max_wall_seconds: 900\n\
         - escalation: autonomous\n\
         \n"
    ));
    s.push_str(
        "TOPOLOGY SELECTION — pick per child by task signature:\n\
         - agentry-spec-edit-v0  → specs/* or docs/* changes only, no Rust code touched\n\
         - agentry-bugfix-v0     → sub-30-LOC bug fix in Rust, no new types/traits, no spec change\n\
         - agentry-self-host-v0  → everything else (default; new features, schema changes, multi-file refactors)\n\
         \n",
    );
    s.push_str(&format!(
        "Output EXACTLY one JSON array — no markdown fences, no prose. Cap at\n\
         {max_children} elements. Schema per element:\n\
         \n\
         {{\n  \
           \"title\": \"<short verb-payload title>\",\n  \
           \"topology\": \"agentry-self-host-v0\" | \"agentry-bugfix-v0\" | \"agentry-spec-edit-v0\",\n  \
           \"verbs\": \"<full verb-payload markdown using CREATE/UPDATE/REPLACE/DELETE/MOVE>\",\n  \
           \"acceptance\": \"<bash command, e.g. cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace>\",\n  \
           \"estimated_files\": [\"<crate>:<file>\"]\n\
         }}\n\
         \n\
         Your response, right now, starting with [ and ending with ]:\n"
    ));
    s
}

/// Strip optional code fences, slice between the first `[` and last `]`,
/// parse as a JSON array. Returns `None` for prose, objects, or
/// unparseable JSON. Mirrors the bash fence-strip + bracket-slice pipeline.
pub fn parse_planner_response(raw: &str) -> Option<Vec<Value>> {
    let cleaned = strip_fences(raw);
    let sliced = slice_json_array(&cleaned)?;
    let v: Value = serde_json::from_str(sliced).ok()?;
    match v {
        Value::Array(arr) => Some(arr),
        _ => None,
    }
}

/// Cap a parsed array of child descriptors to `max_children` entries.
/// Mirrors bash `jq --argjson n "$max_children" '.[:$n]'`.
pub fn cap_children(elements: Vec<Value>, max_children: u64) -> Vec<Value> {
    let cap = max_children as usize;
    if elements.len() > cap {
        elements.into_iter().take(cap).collect()
    } else {
        elements
    }
}

/// Build one child brief JSON document from a planner-emitted element.
///
/// `elem_topology` falls back to `default_topology` when the array element
/// omits `topology` or carries `null`. Mirrors the bash:
/// ```sh
/// elem_topology=$(printf '%s' "$elem" | jq -r '.topology // empty')
/// if [ -z "$elem_topology" ] || [ "$elem_topology" = "null" ]; then
///     elem_topology="$child_topology"
/// fi
/// ```
pub fn build_child_brief(
    brief_id: &str,
    index: usize,
    elem: &Value,
    default_topology: &str,
    target_repo: &str,
    base_branch: &str,
    submitted_at: &str,
) -> Value {
    let title = elem
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let verbs = elem
        .get("verbs")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let acceptance = elem
        .get("acceptance")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let elem_topology = elem
        .get("topology")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_topology)
        .to_string();

    let pr_title = format!("auto(planner-{brief_id}): {title}");
    let pr_body =
        format!("Authored by planner-claude-agentry from meta-brief {brief_id}. Verbs:\n\n{verbs}");

    json!({
        "id": format!("brf_planner_{brief_id}_child_{index}"),
        "project": Value::Null,
        "topology": {"name": elem_topology, "version": 1},
        "payload": {
            "issue_number": 0,
            "issue_title": title,
            "issue_body": verbs,
            "acceptance": acceptance,
            "target_repo": target_repo,
            "base_branch": base_branch,
            "pr_title": pr_title,
            "pr_body": pr_body,
        },
        "budget": {"max_wall_seconds": 900},
        "escalation": "autonomous",
        "parent_brief": brief_id,
        "submitted_by": format!("planner-claude-agentry-{brief_id}"),
        "submitted_at": submitted_at,
    })
}

/// Convenience wrapper that uses the current UTC time for `submitted_at`.
pub fn build_child_brief_now(
    brief_id: &str,
    index: usize,
    elem: &Value,
    default_topology: &str,
    target_repo: &str,
    base_branch: &str,
) -> Value {
    build_child_brief(
        brief_id,
        index,
        elem,
        default_topology,
        target_repo,
        base_branch,
        &Utc::now().to_rfc3339(),
    )
}
