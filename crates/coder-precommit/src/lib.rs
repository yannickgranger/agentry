//! Pre-commit gates and AC verification binaries used by the coder role.

pub mod ac_verifier;
pub mod git_operator;
pub mod providers;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, PartialEq, Eq)]
pub struct AddedPubItem {
    pub file: String,
    pub line: u32,
    pub kind: String,
    pub name: String,
}

pub fn parse_added_pub_items(diff: &str) -> Vec<AddedPubItem> {
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
