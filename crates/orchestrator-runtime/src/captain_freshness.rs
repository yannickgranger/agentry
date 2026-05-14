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

/// Classification of a single file:line ref against the target repo.
///
/// Variant names mirror the operator-facing stdout column values
/// (`OK` / `MISSING` / `LINE_OUT_OF_RANGE`). `pub` because integration
/// tests under `tests/` need to construct/match it — `pub(crate)` would
/// not be visible across the crate boundary, and inline `#[cfg(test)]`
/// modules in `src/` are banned by `arch-ban-inline-cfg-test-in-src.cypher`.
#[derive(Debug, PartialEq)]
pub enum RefStatus {
    Ok,
    Missing,
    OutOfRange {
        actual_lines: u64,
    },
    Renamed {
        name: String,
        expected_file: String,
        actual_file: String,
    },
}

/// One row from a cfdb `query` invocation: the qname-plus-file projection
/// the pub-name probe consumes. `pub` so unit tests can construct fixtures
/// without going through the cfdb subprocess.
#[derive(Debug, PartialEq)]
pub struct CfdbRow {
    pub qname: String,
    pub file: String,
}

/// English words that look like CamelCase / snake_case identifiers but are
/// just prose markers — skip them in [`parse_pub_name_refs`] so they don't
/// pollute the probe set.
const EN_ALLOWLIST: &[&str] = &["TODO", "FIXME", "NOTE", "OK", "MISSING"];

/// Pure line-count comparison: classify already-fetched content against
/// an optional expected line number.
///
/// Returns `OutOfRange { actual_lines }` when `expected_line` is `Some(n)`
/// and `n` exceeds the newline-delimited line count of `content`;
/// otherwise `Ok`. `pub` so unit tests can exercise the comparison
/// without HTTP mocking (see [[classify_ref]] for the HTTP-fronted wrapper).
pub fn classify_against_content(content: &str, expected_line: Option<u64>) -> RefStatus {
    let line_count = content.lines().count() as u64;
    match expected_line {
        Some(n) if n > line_count => RefStatus::OutOfRange {
            actual_lines: line_count,
        },
        _ => RefStatus::Ok,
    }
}

/// Probe a single file ref against the forge contents endpoint and
/// classify the result.
///
/// Returns `Ok(RefStatus::Missing)` on HTTP 404, propagates `Err` on any
/// other non-2xx, otherwise base64-decodes the response and delegates
/// the line-count comparison to [`classify_against_content`]. `pub` so
/// integration tests can drive it directly (the inline-`cfg(test)` ban
/// rules out the alternative).
pub fn classify_ref(
    client: &reqwest::blocking::Client,
    token: &str,
    forge_host: &str,
    target_repo: &str,
    base_branch: &str,
    path: &str,
    expected_line: Option<u64>,
) -> Result<RefStatus> {
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
        return Ok(RefStatus::Missing);
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
    Ok(classify_against_content(&content, expected_line))
}

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

/// Extract pub-name refs cited in a forge issue body.
///
/// Scans the body for backtick-quoted spans and matches an identifier when
/// the delimited content is either CamelCase (an uppercase letter followed
/// by one or more alphanumeric characters) or snake_case (a lowercase
/// letter or underscore followed by lowercase, digit, or underscore
/// characters). For each match, searches backward in the body within the
/// preceding five lines for the nearest file ref matching the
/// `parse_file_refs` regex; if found, pairs them — else pairs with `None`.
///
/// Identifiers whose name matches [`EN_ALLOWLIST`] case-insensitive are
/// skipped. The output is deduplicated by name, keeping the first
/// occurrence in source order.
pub fn parse_pub_name_refs(body: &str) -> Vec<(String, Option<String>)> {
    let backtick_re =
        Regex::new(r"`([^`]+)`").expect("captain_freshness backtick regex must compile");
    let ident_re = Regex::new(r"^(?:[A-Z][A-Za-z0-9]+|[a-z_][a-z0-9_]*)$")
        .expect("captain_freshness ident regex must compile");
    let path_re =
        Regex::new(r"\b(crates?/[A-Za-z0-9_./-]+\.(?:rs|toml|md|sh|json|yml|yaml))(?::(\d+))?\b")
            .expect("captain_freshness path regex must compile");

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<(String, Option<String>)> = Vec::new();

    for caps in backtick_re.captures_iter(body) {
        let Some(name_match) = caps.get(1) else {
            continue;
        };
        let name = name_match.as_str();
        if !ident_re.is_match(name) {
            continue;
        }
        if EN_ALLOWLIST
            .iter()
            .any(|allow| allow.eq_ignore_ascii_case(name))
        {
            continue;
        }
        if seen.contains(name) {
            continue;
        }

        let start = name_match.start();
        let before = &body[..start];
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let mut window_start = line_start;
        for _ in 0..4 {
            if window_start == 0 {
                break;
            }
            let prior = &body[..window_start - 1];
            window_start = prior.rfind('\n').map(|i| i + 1).unwrap_or(0);
        }
        let after = &body[start..];
        let line_end = start + after.find('\n').unwrap_or(after.len());
        let window = &body[window_start..line_end];
        let ident_in_window = start - window_start;
        let nearest_path = path_re
            .captures_iter(window)
            .filter_map(|c| c.get(1))
            .min_by_key(|m| {
                let mid = (m.start() + m.end()) / 2;
                mid.abs_diff(ident_in_window)
            })
            .map(|m| m.as_str().to_string());

        seen.insert(name.to_string());
        out.push((name.to_string(), nearest_path));
    }

    out
}

/// Pure verdict for a pub-name probe against an optional cfdb row.
///
/// `row = None` → `Missing` (cfdb has no qname ending in `::<name>`).
/// `row = Some(r)` with `expected_file = Some(p)` and `r.file != p` →
/// `Renamed { name, expected_file: p, actual_file: r.file }`.
/// Otherwise → `Ok`. Pure helper so unit tests can exercise the
/// classification without driving the cfdb subprocess.
pub fn classify_pub_name_against_row(
    name: &str,
    expected_file: Option<&str>,
    row: Option<&CfdbRow>,
) -> RefStatus {
    let Some(row) = row else {
        return RefStatus::Missing;
    };
    if let Some(p) = expected_file {
        if row.file != p {
            return RefStatus::Renamed {
                name: name.to_string(),
                expected_file: p.to_string(),
                actual_file: row.file.clone(),
            };
        }
    }
    RefStatus::Ok
}

/// Probe a single pub-name against a pre-populated cfdb cache.
///
/// Shells out to `cfdb query` with a Cypher pattern that matches Items
/// whose `qname` ends with `::<name>` (regex anchored at end-of-qname
/// after a `::` separator). Parses stdout JSON; if `rows` is empty
/// delegates to [`classify_pub_name_against_row`] with `None`, else
/// takes the first row's `qname`/`file` strings, builds a [`CfdbRow`],
/// and delegates.
pub fn probe_pub_name(
    name: &str,
    expected_file: Option<&str>,
    cfdb_db: &std::path::Path,
    keyspace: &str,
) -> Result<RefStatus> {
    let cfdb_db_str = cfdb_db.to_string_lossy().to_string();
    let mut cypher = String::from(r#"MATCH (i:Item) WHERE i.qname =~ ".*"#);
    cypher.push_str("::");
    cypher.push_str(name);
    cypher.push_str(r#"$" RETURN i.qname, i.file LIMIT 1"#);

    let output = std::process::Command::new("cfdb")
        .args([
            "query",
            "--db",
            &cfdb_db_str,
            "--keyspace",
            keyspace,
            &cypher,
        ])
        .output()
        .with_context(|| format!("invoke cfdb query for pub-name `{name}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(anyhow!("cfdb query failed for pub-name `{name}`: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("parse cfdb query stdout JSON for pub-name `{name}`"))?;
    let rows = parsed.get("rows").and_then(|v| v.as_array());
    let row = rows.and_then(|r| r.first()).and_then(|first| {
        let qname = first.get("qname").and_then(|v| v.as_str())?;
        let file = first.get("file").and_then(|v| v.as_str())?;
        Some(CfdbRow {
            qname: qname.to_string(),
            file: file.to_string(),
        })
    });
    Ok(classify_pub_name_against_row(
        name,
        expected_file,
        row.as_ref(),
    ))
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
    let mut renamed = 0usize;

    for (path, line) in &refs {
        let display = match line {
            Some(n) => format!("{path}:{n}"),
            None => path.clone(),
        };
        match classify_ref(
            &client,
            &token,
            forge_host,
            target_repo,
            base_branch,
            path,
            *line,
        )? {
            RefStatus::Ok => {
                ok += 1;
                println!("{display}\tOK\t");
            }
            RefStatus::Missing => {
                missing += 1;
                println!("{display}\tMISSING\t{path}");
            }
            RefStatus::OutOfRange { actual_lines } => {
                oor += 1;
                println!("{display}\tLINE_OUT_OF_RANGE\tactual {actual_lines} lines");
            }
            RefStatus::Renamed { .. } => unreachable!(
                "file:line probe must not produce Renamed; it is a pub-name probe verdict"
            ),
        }
    }

    let slug = target_repo.replace('/', "_");
    let branch_url =
        format!("https://{forge_host}/api/v1/repos/{target_repo}/branches/{base_branch}");
    let mut skip_pub_name = false;
    let mut rev = String::new();
    match client
        .get(&branch_url)
        .header("Authorization", format!("token {token}"))
        .send()
    {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                match resp.json::<serde_json::Value>() {
                    Ok(json) => {
                        match json
                            .get("commit")
                            .and_then(|c| c.get("id"))
                            .and_then(|v| v.as_str())
                        {
                            Some(sha) => rev = sha.to_string(),
                            None => {
                                eprintln!(
                                    "(pub-name probe skipped: branch tip SHA missing from {branch_url} response)"
                                );
                                skip_pub_name = true;
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("(pub-name probe skipped: parse branch JSON failed: {err})");
                        skip_pub_name = true;
                    }
                }
            } else {
                eprintln!("(pub-name probe skipped: GET {branch_url} returned {status})");
                skip_pub_name = true;
            }
        }
        Err(err) => {
            eprintln!("(pub-name probe skipped: GET {branch_url} failed: {err})");
            skip_pub_name = true;
        }
    }

    if !skip_pub_name {
        let cache_dir = crate::captain_ground_cache::captain_ground_cache_dir(
            &slug,
            &rev,
            std::env::var("XDG_CACHE_HOME").ok(),
            std::env::var("HOME").ok(),
        );
        let ground_json = cache_dir.join("ground.json");
        if !ground_json.is_file() {
            eprintln!(
                "(pub-name probe skipped: no cfdb cache at {}; run captain ground --target-repo {target_repo} to populate)",
                ground_json.display()
            );
        } else {
            for (name, expected_file) in parse_pub_name_refs(body) {
                let status = probe_pub_name(&name, expected_file.as_deref(), &cache_dir, "ground")?;
                match status {
                    RefStatus::Ok => {
                        ok += 1;
                        println!("{name}\tOK\t");
                    }
                    RefStatus::Missing => {
                        missing += 1;
                        println!("{name}\tMISSING\t");
                    }
                    RefStatus::Renamed {
                        expected_file,
                        actual_file,
                        ..
                    } => {
                        renamed += 1;
                        println!(
                            "{name}\tRENAMED\texpected {expected_file}, actual {actual_file}"
                        );
                    }
                    RefStatus::OutOfRange { .. } => unreachable!(
                        "pub-name probe must not produce OutOfRange; it is a file:line probe verdict"
                    ),
                }
            }
        }
    }

    let total = ok + missing + oor + renamed;
    eprintln!(
        "freshness: {total} refs probed — {ok} OK, {missing} MISSING, {oor} LINE_OUT_OF_RANGE, {renamed} RENAMED"
    );

    if missing == 0 && oor == 0 && renamed == 0 {
        Ok(0)
    } else {
        Ok(1)
    }
}
