//! Pre-commit callers gate (Phase 2.3 of #87).
//!
//! Rejects newly-introduced `pub` items that have zero in-repo callers (orphan
//! API surface) unless they are listed in `docs/public_api_allowlist.toml`.
//! Pure-function helpers live here so the runner can compose the gate against
//! `ra-query` without leaking I/O concerns into the predicate, and so the
//! parser, allowlist loader, and decision shape are all unit-testable without
//! a `ra-query` binary or a real workspace.

use orchestrator_types::{FindingOrigin, ReviewFinding, Severity};

/// Source / tool string embedded in every blocker finding emitted by the gate.
pub const GATE_SOURCE: &str = "pre_commit_callers_gate";

/// Category string the gate uses on its blocker findings — matches the brief's
/// finding contract exactly.
pub const GATE_CATEGORY: &str = "orphan_pub_item";

/// One newly-introduced pub item discovered by parsing a unified diff.
///
/// `bare_name` is what gets passed to `ra-query callers <name>` (matching the
/// existing `dead-pub-check` binary). `fqn` is the workspace-stable identifier
/// matched against the allowlist — `<crate_name>::<bare_name>` for files under
/// `crates/<dir>/...`, falling back to the bare name otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPubItem {
    pub kind: String,
    pub bare_name: String,
    pub fqn: String,
    pub file: String,
    pub line: u32,
}

/// Public-API allowlist parsed from `docs/public_api_allowlist.toml`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PublicApiAllowlist {
    pub names: Vec<String>,
}

impl PublicApiAllowlist {
    pub fn contains(&self, fqn: &str) -> bool {
        self.names.iter().any(|n| n == fqn)
    }
}

/// Outcome of evaluating the gate against a set of new pub items.
#[derive(Debug)]
pub enum GateDecision {
    /// `ra-query` is unavailable; the gate is opt-in via the binary's presence.
    SkippedNoRaQuery,
    /// A `ra-query callers` invocation failed mid-flight; degrade open.
    SkippedResolverFailed(String),
    /// No blockers fired.
    Clean,
    /// One or more orphan pubs detected — these are emitted as Blocker findings
    /// and the runner emits `done failed` with `pre_commit_callers_gate_failed`.
    BlockersFired(Vec<ReviewFinding>),
}

/// True iff the item should fire a Blocker — zero callers AND not on the
/// allowlist.
pub fn is_orphan_pub_item(
    item: &NewPubItem,
    callers_count: usize,
    allowlist: &PublicApiAllowlist,
) -> bool {
    callers_count == 0 && !allowlist.contains(&item.fqn)
}

/// Compose the orphan-pub Blocker finding for one item.
pub fn orphan_pub_item_finding(item: &NewPubItem) -> ReviewFinding {
    ReviewFinding {
        file: Some(item.file.clone()),
        line: Some(item.line),
        severity: Severity::Blocker,
        origin: FindingOrigin::Mechanical {
            tool: GATE_SOURCE.into(),
            rule: Some(GATE_CATEGORY.into()),
        },
        category: GATE_CATEGORY.into(),
        message: format!(
            "{} at {}:{} has zero callers and is not on the public-API allowlist; either add a caller, mark it pub(crate), or add to the allowlist",
            item.fqn, item.file, item.line,
        ),
        suggested_fix: None,
        prohibitions: Vec::new(),
        requirements: Vec::new(),
    }
}

/// Pure decision over the gate inputs. The runner wires `ra_query_available`
/// from a presence probe and `resolver` from `ra-query callers <name>`; this
/// function returns the shape the runner pattern-matches on to decide whether
/// to continue, skip, or emit `done failed`.
pub fn decide_gate<F>(
    ra_query_available: bool,
    items: &[NewPubItem],
    allowlist: &PublicApiAllowlist,
    mut resolver: F,
) -> GateDecision
where
    F: FnMut(&str) -> Result<usize, String>,
{
    if !ra_query_available {
        return GateDecision::SkippedNoRaQuery;
    }
    let mut findings = Vec::new();
    for item in items {
        match resolver(&item.bare_name) {
            Ok(count) => {
                if is_orphan_pub_item(item, count, allowlist) {
                    findings.push(orphan_pub_item_finding(item));
                }
            }
            Err(e) => return GateDecision::SkippedResolverFailed(e),
        }
    }
    if findings.is_empty() {
        GateDecision::Clean
    } else {
        GateDecision::BlockersFired(findings)
    }
}

/// Parse the allowlist TOML. The shape is restricted on purpose: a single
/// `[allowed]` table with a `names = ["a", "b", ...]` array of strings. Any
/// other shape is treated as malformed — the function returns an empty
/// allowlist plus a short reason string the caller can surface as a warning
/// event, so the gate degrades to "block all orphans" rather than silently
/// allowing them through.
pub fn parse_allowlist_toml(raw: &str) -> (PublicApiAllowlist, Option<String>) {
    let mut in_allowed = false;
    let mut saw_allowed_table = false;
    let mut names_block = String::new();
    let mut accumulating = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_allowed = trimmed == "[allowed]";
            if in_allowed {
                saw_allowed_table = true;
            }
            accumulating = false;
            continue;
        }
        if !in_allowed {
            continue;
        }
        if accumulating || trimmed.starts_with("names") {
            accumulating = true;
            names_block.push_str(line);
            names_block.push('\n');
            if trimmed.ends_with(']') {
                accumulating = false;
            }
        }
    }
    if !saw_allowed_table {
        return (
            PublicApiAllowlist::default(),
            Some("missing [allowed] table".into()),
        );
    }
    if names_block.is_empty() {
        // The table exists but has no `names` key — treat as empty list, not
        // an error.
        return (PublicApiAllowlist::default(), None);
    }
    let start = match names_block.find('[') {
        Some(i) => i,
        None => {
            return (
                PublicApiAllowlist::default(),
                Some("malformed names array: missing '['".into()),
            );
        }
    };
    let end = match names_block.rfind(']') {
        Some(i) => i,
        None => {
            return (
                PublicApiAllowlist::default(),
                Some("malformed names array: missing ']'".into()),
            );
        }
    };
    if end <= start {
        return (
            PublicApiAllowlist::default(),
            Some("malformed names array: ']' precedes '['".into()),
        );
    }
    let inner = &names_block[start + 1..end];
    let mut names = Vec::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut s = String::new();
            let mut closed = false;
            for cc in chars.by_ref() {
                if cc == '"' {
                    closed = true;
                    break;
                }
                s.push(cc);
            }
            if !closed {
                return (
                    PublicApiAllowlist::default(),
                    Some("malformed names array: unterminated string".into()),
                );
            }
            names.push(s);
        }
    }
    (PublicApiAllowlist { names }, None)
}

/// Parse a unified diff for newly-introduced `pub` items. Recognised forms:
/// `pub fn|struct|enum|trait|type|const|static|mod <name>` and
/// `pub use <path>[ as <name>]`. `pub(crate)` / `pub(super)` / `pub(in path)`
/// are intentionally skipped — not publicly-visible API surface.
pub fn parse_new_pub_items(diff: &str) -> Vec<NewPubItem> {
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
                let fqn = derive_fqn(file, name);
                items.push(NewPubItem {
                    kind: kind.to_string(),
                    bare_name: name.to_string(),
                    fqn,
                    file: file.clone(),
                    line: new_line,
                });
            }
            new_line += 1;
        } else if raw.starts_with('-') {
            // deletion line — does not advance new-file line counter
        } else {
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
    for kind in [
        "fn", "struct", "enum", "trait", "type", "const", "static", "mod",
    ] {
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
    if let Some(rest) = after_pub.strip_prefix("use ") {
        let path = rest.trim_end_matches(';').trim();
        if let Some(idx) = path.rfind(" as ") {
            let after_as = path[idx + 4..].trim();
            let name_end = after_as
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after_as.len());
            if name_end == 0 {
                return None;
            }
            let name = &after_as[..name_end];
            let first = name.chars().next()?;
            if !(first.is_ascii_alphabetic() || first == '_') {
                return None;
            }
            return Some(("use", name));
        }
        let last = path.rsplit("::").next()?;
        let name_end = last
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(last.len());
        if name_end == 0 {
            return None;
        }
        let name = &last[..name_end];
        let first = name.chars().next()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        return Some(("use", name));
    }
    None
}

/// Derive `<crate_name>::<bare_name>` from a workspace-relative file path
/// matching `crates/<dir>/...`. `<dir>` has hyphens converted to underscores
/// so the FQN matches Rust crate-identifier conventions. For paths that do
/// not match the pattern, return the bare name unchanged.
pub fn derive_fqn(file: &str, bare_name: &str) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    if parts.len() >= 3 && parts[0] == "crates" {
        let crate_name = parts[1].replace('-', "_");
        return format!("{crate_name}::{bare_name}");
    }
    bare_name.to_string()
}
