//! captain freshness — probe file:line refs cited in a forge issue body
//! against `target_repo@base_branch` to catch drift between issue filing
//! and brief authoring (file split / renamed / moved, line ranges shifted).
//!
//! v1 scope: parse `crates?/<path>.<ext>[:N]` refs out of the issue body,
//! GET the contents endpoint for each, and classify as `OK`, `MISSING`
//! (404), or `LINE_OUT_OF_RANGE { actual_lines }`. Out of scope: cfdb
//! pub-name verification and N-consumer counting (separate follow-ups).
//!
//! The `parse_file_refs` helper is factored out so unit tests can drive
//! the regex directly without spinning up a network mock.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use regex::Regex;
use std::collections::HashSet;

/// Extract file/line refs cited in a forge issue body.
///
/// Returns each distinct `(path, Option<line>)` pair in source order, with
/// later duplicates dropped. The regex requires the `crates?/` prefix so
/// bare filenames like `README.md` are skipped. The optional `:N` line
/// number is captured when present; the line-only-form is preserved
/// distinct from the no-line form.
pub fn parse_file_refs(body: &str) -> Vec<(String, Option<u64>)> {
    let re = Regex::new(
        r"(?m)\b(crates?/[A-Za-z0-9_./-]+\.(?:rs|toml|md|sh|json|yml|yaml))(?::(\d+))?\b",
    )
    .expect("captain_freshness regex must compile");
    let mut seen: HashSet<(String, Option<u64>)> = HashSet::new();
    let mut refs: Vec<(String, Option<u64>)> = Vec::new();
    for caps in re.captures_iter(body) {
        let Some(path_match) = caps.get(1) else {
            continue;
        };
        let path = path_match.as_str().to_string();
        let line = caps.get(2).and_then(|m| m.as_str().parse::<u64>().ok());
        let key = (path.clone(), line);
        if seen.insert(key) {
            refs.push((path, line));
        }
    }
    refs
}

/// Run the freshness probe end-to-end: fetch the issue body, parse refs,
/// probe each against `target_repo@base_branch` via the forge contents
/// endpoint, print a table to stdout, print a summary to stderr, and
/// return exit code 0 (all clean) or 1 (any MISSING / LINE_OUT_OF_RANGE).
pub fn run_freshness(
    target_repo: &str,
    issue: u64,
    base_branch: &str,
    forge_host: &str,
) -> Result<i32> {
    let token = std::env::var("GITEA_TOKEN")
        .context("GITEA_TOKEN env var required for forge issue fetch")?;
    let client = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build blocking reqwest client")?;

    let issue_url = format!("https://{forge_host}/api/v1/repos/{target_repo}/issues/{issue}");
    let issue_resp = client
        .get(&issue_url)
        .header("Authorization", format!("token {token}"))
        .send()
        .with_context(|| format!("GET {issue_url}"))?;
    let issue_status = issue_resp.status();
    if !issue_status.is_success() {
        let detail = issue_resp.text().unwrap_or_default();
        return Err(anyhow!(
            "gitea issue fetch failed: {issue_status} — {detail}"
        ));
    }
    let issue_json: serde_json::Value = issue_resp.json().context("parse issue JSON")?;
    let body = issue_json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut refs = parse_file_refs(body);
    refs.sort();

    let mut ok = 0usize;
    let mut missing = 0usize;
    let mut oor = 0usize;

    for (path, line) in &refs {
        let display = match line {
            Some(n) => format!("{path}:{n}"),
            None => path.clone(),
        };
        let contents_url = format!(
            "https://{forge_host}/api/v1/repos/{target_repo}/contents/{path}?ref={base_branch}"
        );
        let resp = client
            .get(&contents_url)
            .header("Authorization", format!("token {token}"))
            .send()
            .with_context(|| format!("GET {contents_url}"))?;
        let status = resp.status();
        if status.as_u16() == 404 {
            missing += 1;
            println!("{display}\tMISSING\t{path}");
            continue;
        }
        if !status.is_success() {
            let detail = resp.text().unwrap_or_default();
            return Err(anyhow!(
                "gitea contents fetch failed for {path}: {status} — {detail}"
            ));
        }
        let body_json: serde_json::Value = resp
            .json()
            .with_context(|| format!("parse contents JSON for {path}"))?;
        let encoded = body_json
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned.as_bytes())
            .with_context(|| format!("decode base64 content for {path}"))?;
        let content = String::from_utf8_lossy(&decoded_bytes);
        let line_count = content.lines().count() as u64;
        match line {
            Some(n) if *n > line_count => {
                oor += 1;
                println!("{display}\tLINE_OUT_OF_RANGE\tactual {line_count} lines");
            }
            _ => {
                ok += 1;
                println!("{display}\tOK\t");
            }
        }
    }

    let total = refs.len();
    eprintln!(
        "freshness: {total} refs probed — {ok} OK, {missing} MISSING, {oor} LINE_OUT_OF_RANGE"
    );

    if missing == 0 && oor == 0 {
        Ok(0)
    } else {
        Ok(1)
    }
}
