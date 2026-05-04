//! Pure helpers for the archaeologist-claude-runner binary (EPIC #161
//! Wave 3). Lives in the lib crate so the test crate at
//! `crates/agentry-role-runtime/tests/archaeologist_runner_test.rs` can
//! reach it without touching the `src/bin/` file (the
//! `arch-ban-inline-cfg-test-in-src` rule forbids inline `#[cfg(test)]`
//! modules under `src/`).

use serde_json::Value;

use crate::{head_bytes, slice_json_object, strip_fences};

/// Maximum bytes of `graph-specs` output inlined into the prompt.
/// Mirrors the bash `head -c 4000`.
pub const GRAPH_SPECS_HEAD: usize = 4000;

/// Parse the canonical `extract: N nodes, M edges` log line emitted by
/// `cfdb extract`. Mirrors the bash:
///
/// ```sh
/// counts_line=$(grep -E 'extract: [0-9]+ nodes' /tmp/cfdb-extract.log | tail -1)
/// nodes=$(printf '%s' "$counts_line" | sed -nE 's/.*extract: ([0-9]+) nodes.*/\1/p')
/// edges=$(printf '%s' "$counts_line" | sed -nE 's/.*nodes, ([0-9]+) edges.*/\1/p')
/// ```
///
/// When multiple lines match, the last one wins (bash `tail -1`). Both
/// fields default to `0` if the pattern is absent or partially missing.
pub fn parse_cfdb_counts(log: &str) -> (u64, u64) {
    let mut nodes: u64 = 0;
    let mut edges: u64 = 0;
    for line in log.lines() {
        let Some(after_marker_idx) = line.find("extract: ") else {
            continue;
        };
        let after = &line[after_marker_idx + "extract: ".len()..];
        let n_end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if n_end == 0 {
            continue;
        }
        let rest_after_n = &after[n_end..];
        if !rest_after_n.starts_with(" nodes") {
            continue;
        }
        let n: u64 = after[..n_end].parse().unwrap_or(0);
        // Reset edges per matched line — the last hit owns both fields.
        let mut e_match: u64 = 0;
        if let Some(comma_off) = rest_after_n.find(", ") {
            let after_comma = &rest_after_n[comma_off + 2..];
            let e_end = after_comma
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after_comma.len());
            if e_end > 0 && after_comma[e_end..].starts_with(" edges") {
                e_match = after_comma[..e_end].parse().unwrap_or(0);
            }
        }
        nodes = n;
        edges = e_match;
    }
    (nodes, edges)
}

/// Pull the `discovery_seeds` array out of a startup bundle. Mirrors bash
/// `jq -c '.brief.payload.discovery_seeds // []'` followed by the
/// `while jq -r ".[$i]"` loop — non-string entries are silently dropped.
pub fn parse_discovery_seeds(bundle: &Value) -> Vec<String> {
    bundle
        .pointer("/brief/payload/discovery_seeds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Build the archaeologist claude prompt — verbatim port of the bash
/// `cat > /tmp/arch_prompt.txt <<PROMPT` heredoc. `seed_results_json`
/// is inlined raw (it's already a JSON array literal), matching the
/// bash `$seed_results` expansion.
pub fn build_archaeologist_prompt(
    intent: &str,
    success_criteria: &str,
    nodes: u64,
    edges: u64,
    graph_specs_out: &str,
    seed_results_json: &str,
) -> String {
    let graph_head = head_bytes(graph_specs_out, GRAPH_SPECS_HEAD);
    let mut s = String::new();
    s.push_str(
        "You are the archaeologist role for the agentry project. Synthesize a\n\
         discovery.json for downstream planner consumption based on the inputs below.\n\
         \n",
    );
    s.push_str(&format!("INTENT:\n{intent}\n\n"));
    s.push_str(&format!("SUCCESS CRITERIA:\n{success_criteria}\n\n"));
    s.push_str(&format!(
        "CFDB EXTRACT SUMMARY:\nnodes={nodes}, edges={edges}\n\n"
    ));
    s.push_str(&format!(
        "GRAPH-SPECS OUTPUT (first 4000 chars):\n{graph_head}\n\n"
    ));
    s.push_str(&format!(
        "SEED-QUERY RESULTS (JSON):\n{seed_results_json}\n\n"
    ));
    s.push_str("Output EXACTLY one JSON object — no markdown fences, no prose. Schema:\n\n");
    s.push_str("{\n");
    s.push_str("  \"intent\": \"<copied verbatim from INTENT above>\",\n");
    s.push_str(
        "  \"summary\": \"<1-3 sentence narrative about workspace state relative to intent>\",\n",
    );
    s.push_str("  \"raw_facts\": {\n");
    s.push_str(&format!(
        "    \"cfdb\": {{\"nodes\": {nodes}, \"edges\": {edges}}},\n"
    ));
    s.push_str("    \"graph_specs_violations\": [<pass-through of any violations parsed from GRAPH-SPECS OUTPUT, or []>],\n");
    s.push_str(&format!("    \"seed_queries\": {seed_results_json}\n"));
    s.push_str("  },\n");
    s.push_str("  \"candidates\": [\n");
    s.push_str("    {\"target\": \"<qname or file:line>\", \"kind\": \"<reuse|extend|create|fix>\", \"rationale\": \"<short>\"}\n");
    s.push_str("  ],\n");
    s.push_str("  \"success_criteria\": \"<copied verbatim from SUCCESS CRITERIA above, or empty string>\"\n");
    s.push_str("}\n\n");
    s.push_str("Your response, right now, starting with { and ending with }:\n");
    s
}

/// Strip optional code fences, slice between the first `{` and last `}`,
/// parse as a JSON object. Returns `None` for prose, arrays, or
/// unparseable JSON. Mirrors the bash:
///
/// ```sh
/// cleaned=$(printf '%s' "$response" | sed -e 's/^```json$//' -e 's/^```$//' -e '/^$/d' | tr -d '\r')
/// start=$(printf '%s' "$cleaned" | grep -b -m1 '{' | head -1 | cut -d: -f1)
/// end=$(printf '%s' "$cleaned" | grep -bo '}' | tail -1 | cut -d: -f1)
/// payload=$(printf '%s' "$cleaned" | tail -c +$((start+1)) | head -c $((end-start+1)))
/// printf '%s' "$payload" | jq -e 'type == "object"'
/// ```
pub fn parse_discovery_object(raw: &str) -> Option<Value> {
    let cleaned = strip_fences(raw);
    let sliced = slice_json_object(&cleaned)?;
    let v: Value = serde_json::from_str(sliced).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(v)
}
