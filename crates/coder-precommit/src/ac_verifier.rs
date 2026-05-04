//! AC-verifier core. Pure logic — I/O and process spawning live in the binary
//! and the `providers::ClaudeProvider` impl. Run via `run(input, &provider)`
//! and matched on `Outcome`. Degrades to `Outcome::Shipped` whenever the
//! provider errors, returns invalid JSON, or AC list is missing/empty:
//! reviewer-claude is the architectural backstop.

use serde::Deserialize;

use crate::providers::AcVerifierProvider;

#[derive(Debug, Deserialize)]
pub struct Input {
    pub acceptance_criteria: Option<Vec<String>>,
    pub diff: String,
    pub verb_body: String,
}

#[derive(Debug, Deserialize)]
struct Verdict {
    ac: String,
    verdict: String,
    evidence: String,
}

#[derive(Debug, Deserialize)]
struct ProviderResponse {
    verdicts: Vec<Verdict>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: String,
    pub category: String,
    pub message: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Shipped,
    Rework { findings: Vec<Finding> },
}

const SYSTEM_PROMPT: &str = "You are the AC-verifier role inside an autonomous coding pipeline. \
You receive a list of acceptance criteria, the coder's git diff, and the brief's verb body. \
For each acceptance criterion you must judge whether the diff clearly meets it (passed), \
clearly does not meet it (failed), or is ambiguous (uncertain). \
\n\nOutput schema (strict): respond with a single JSON object and no prose, no markdown, no fences:\n\
{\"verdicts\":[{\"ac\":\"<verbatim AC text>\",\"verdict\":\"passed|failed|uncertain\",\"evidence\":\"<short justification grounded in the diff>\"}, ...]}\n\
\nPolicy:\n\
- failed = the AC is clearly not met by the diff (a hard blocker).\n\
- uncertain = ambiguous, partial, or you cannot determine from the diff alone.\n\
- passed = clearly met by the diff.\n\
Treat uncertain as non-blocking — only failed produces a rework finding.";

pub fn run<P: AcVerifierProvider>(input: Input, provider: &P) -> Outcome {
    let acs = match &input.acceptance_criteria {
        Some(v) if !v.is_empty() => v,
        _ => return Outcome::Shipped,
    };

    let mut user = String::new();
    user.push_str("ACCEPTANCE_CRITERIA:\n");
    for (i, ac) in acs.iter().enumerate() {
        user.push_str(&format!("{}. {}\n", i + 1, ac));
    }
    user.push_str("\nDIFF:\n");
    user.push_str(&input.diff);
    user.push_str("\n\nVERB_BODY:\n");
    user.push_str(&input.verb_body);

    let raw = match provider.verify(SYSTEM_PROMPT, &user) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ac-verifier: provider error: {e}");
            return Outcome::Shipped;
        }
    };

    let stripped = strip_json_fence(&raw);
    let parsed: ProviderResponse = match serde_json::from_str(stripped) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ac-verifier: invalid provider JSON: {e}");
            return Outcome::Shipped;
        }
    };

    let findings: Vec<Finding> = parsed
        .verdicts
        .into_iter()
        .filter(|v| v.verdict == "failed")
        .map(|v| Finding {
            severity: "blocker".to_string(),
            category: "ac-violation".to_string(),
            message: format!("AC not met: {} — evidence: {}", v.ac, v.evidence),
        })
        .collect();

    if findings.is_empty() {
        Outcome::Shipped
    } else {
        Outcome::Rework { findings }
    }
}

fn strip_json_fence(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        let rest = rest.trim_start_matches('\n');
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
        return rest.trim();
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.trim_start_matches('\n');
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
        return rest.trim();
    }
    trimmed
}
