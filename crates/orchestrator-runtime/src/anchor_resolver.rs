//! Anchor resolver — resolves [`AssertionAnchor`] variants against the local
//! agentry workspace's cfdb keyspace and `specs/concepts/` directory.
//!
//! Pure helper module. B6b will wire this into daemon intake; this slice
//! lands the resolver and its tests in isolation.
//!
//! The dispatch in [`resolve_assertion`] uses an exhaustive `match` without a
//! wildcard arm: this is the structural fence that forces a deliberate
//! resolver decision when a new [`AssertionAnchor`] variant is added.
//!
//! [`AssertionAnchor`]: orchestrator_types::contract::AssertionAnchor

use orchestrator_types::contract::AssertionAnchor;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Where to look when resolving anchors.
pub struct ResolverContext {
    pub cfdb_db: PathBuf,
    pub cfdb_keyspace: String,
    pub specs_dir: PathBuf,
}

/// Outcome of resolving a single assertion anchor.
pub enum AnchorResolution {
    Resolved,
    NotFound { reason: String },
}

/// Dispatch an [`AssertionAnchor`] to the variant-specific resolver.
///
/// The match is intentionally non-wildcard: adding a new variant in
/// `orchestrator-types` produces a non-exhaustive match compile error here.
pub fn resolve_assertion(anchor: &AssertionAnchor, ctx: &ResolverContext) -> AnchorResolution {
    match anchor {
        AssertionAnchor::Cfdb { qname } => resolve_cfdb_anchor(qname, ctx),
        AssertionAnchor::SpecConcept { path, section } => {
            resolve_spec_concept_anchor(path, section, ctx)
        }
        AssertionAnchor::Behavior { live_target: _ } => AnchorResolution::Resolved,
    }
}

/// Resolve a cfdb qname by shelling out to `cfdb query` and inspecting the
/// JSON `rows` array on stdout.
pub fn resolve_cfdb_anchor(qname: &str, ctx: &ResolverContext) -> AnchorResolution {
    // step 1 — qname injection guard. Reject double-quote so the Cypher
    // string we build below cannot be broken out of.
    if qname.contains('"') {
        return AnchorResolution::NotFound {
            reason: "qname contains illegal character (double quote)".to_string(),
        };
    }

    let cypher = format!(
        "MATCH (i:Item) WHERE i.qname = \"{qname}\" RETURN i.qname",
        qname = qname
    );
    let db_str = ctx.cfdb_db.display().to_string();

    // step 2 — spawn cfdb synchronously, capturing stdout + stderr.
    let output = Command::new("cfdb")
        .args([
            "query",
            "--db",
            &db_str,
            "--keyspace",
            &ctx.cfdb_keyspace,
            &cypher,
        ])
        .output();

    // cfdb may exit non-zero with `EmptyResult` warnings on well-formed empty
    // queries (cf. scripts/arch-check.sh), so exit_status is intentionally
    // NOT used to gate parsing — stdout content is the source of truth.
    let output = match output {
        // Case A — spawn failed.
        Err(io_error) => {
            return AnchorResolution::NotFound {
                reason: format!("cfdb spawn failed: {io_error}"),
            };
        }
        Ok(out) if out.stdout.is_empty() => {
            // Case C — process exited but produced no stdout. Empty stdout
            // cannot be parsed and strongly indicates cfdb failed before
            // emitting any JSON (e.g. missing keyspace, malformed --db).
            let stderr_text = String::from_utf8_lossy(&out.stderr);
            return AnchorResolution::NotFound {
                reason: format!(
                    "cfdb produced no stdout (stderr: {stderr_trimmed}) — likely spawn or db-config error",
                    stderr_trimmed = stderr_text.trim()
                ),
            };
        }
        // Case B — process exited and stdout is non-empty. Parse it.
        Ok(out) => out,
    };

    // step 4 — parse stdout as JSON.
    let value: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(parse_err) => {
            return AnchorResolution::NotFound {
                reason: format!("cfdb stdout is not valid JSON: {parse_err}"),
            };
        }
    };

    let rows = match value.get("rows").and_then(|r| r.as_array()) {
        Some(rows) => rows,
        None => {
            return AnchorResolution::NotFound {
                reason: "cfdb response missing rows array".to_string(),
            };
        }
    };

    // step 5 — interpret rows.
    if rows.is_empty() {
        return AnchorResolution::NotFound {
            reason: format!("cfdb has no Item with qname {qname}"),
        };
    }
    AnchorResolution::Resolved
}

/// Resolve a `specs/concepts/`-relative path + section heading.
pub fn resolve_spec_concept_anchor(
    path: &Path,
    section: &str,
    ctx: &ResolverContext,
) -> AnchorResolution {
    // step 1 — path safety guards.
    if path.is_absolute() {
        return AnchorResolution::NotFound {
            reason: "spec path must be relative".to_string(),
        };
    }
    for comp in path.components() {
        if matches!(comp, Component::ParentDir) {
            return AnchorResolution::NotFound {
                reason: "spec path must contain no parent components".to_string(),
            };
        }
    }

    // step 2 — read the file.
    let full_path = ctx.specs_dir.join(path);
    let body = match std::fs::read_to_string(&full_path) {
        Ok(s) => s,
        Err(e) => {
            return AnchorResolution::NotFound {
                reason: format!("spec file not readable at {}: {e}", full_path.display()),
            };
        }
    };

    // step 3 — heading match. ATX-style headings: one or more '#', then a
    // single ASCII space, then heading text. Compare case-insensitively.
    for line in body.lines() {
        if !line.starts_with('#') {
            continue;
        }
        let stripped = line.trim_start_matches('#');
        // require at least one space after the '#'s (rules out e.g. "#foo")
        if !stripped.starts_with(' ') {
            continue;
        }
        let heading = stripped.trim_start();
        if heading.eq_ignore_ascii_case(section) {
            return AnchorResolution::Resolved;
        }
    }

    // step 4 — no match.
    AnchorResolution::NotFound {
        reason: format!(
            "spec file at {} has no heading matching \"{section}\"",
            full_path.display()
        ),
    }
}
