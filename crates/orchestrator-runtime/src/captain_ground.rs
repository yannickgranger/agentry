//! captain ground — render a grounding sheet from cfdb items + spec matches.
//!
//! The `## Suggested AssertionAnchor seeds` section is built by constructing
//! real `orchestrator_types::contract::AssertionAnchor` values and serializing
//! them via `serde_json`. This is the structural fence that prevents the
//! captain CLI from emitting a hand-authored wire shape that diverges from
//! the type definition: a rename of the `AssertionAnchor` variants in
//! `orchestrator-types` produces a compile error here, not silent drift.

use orchestrator_types::contract::AssertionAnchor;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct CfdbItem {
    pub qname: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub bounded_context: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpecMatch {
    pub file: String,
    pub headings: Vec<String>,
}

pub fn render_grounding_sheet(directive: &str, items: &[CfdbItem], specs: &[SpecMatch]) -> String {
    let mut out = String::new();
    out.push_str("# Grounding sheet\n\n");
    out.push_str(&format!("Directive: {directive}\n\n"));

    out.push_str("## Candidate cfdb qnames\n\n");
    if items.is_empty() {
        out.push_str("(none)\n\n");
    } else {
        for item in items {
            let ctx = item
                .bounded_context
                .clone()
                .unwrap_or_else(|| "none".to_string());
            out.push_str(&format!(
                "- `{}` — {} at {}:{} (context: {})\n",
                item.qname, item.kind, item.file, item.line, ctx
            ));
        }
        out.push('\n');
    }

    out.push_str("## Candidate spec sections\n\n");
    if specs.is_empty() {
        out.push_str("(none)\n\n");
    } else {
        for spec in specs {
            out.push_str(&format!("- {}: {}\n", spec.file, spec.headings.join(";")));
        }
        out.push('\n');
    }

    out.push_str("## Suggested AssertionAnchor seeds\n\n");
    let mut seeds: Vec<AssertionAnchor> = Vec::new();
    for item in items {
        seeds.push(AssertionAnchor::Cfdb {
            qname: item.qname.clone(),
        });
    }
    for spec in specs {
        if let Some(first) = spec.headings.first() {
            seeds.push(AssertionAnchor::SpecConcept {
                path: PathBuf::from(spec.file.clone()),
                section: first.clone(),
            });
        }
    }
    let json = serde_json::to_string_pretty(&seeds).unwrap_or_else(|_| "[]".to_string());
    out.push_str("```json\n");
    out.push_str(&json);
    out.push('\n');
    out.push_str("```\n");

    out
}
