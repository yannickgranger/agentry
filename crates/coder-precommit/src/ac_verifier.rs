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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::MockProvider;

    fn input(acs: Option<Vec<String>>) -> Input {
        Input {
            acceptance_criteria: acs,
            diff: "diff --git a/x b/x\n".to_string(),
            verb_body: "UPDATE x: do y".to_string(),
        }
    }

    #[test]
    fn run_short_circuits_on_none_acceptance() {
        let provider = MockProvider {
            canned_response: "SHOULD_NOT_BE_CALLED".to_string(),
        };
        let outcome = run(input(None), &provider);
        assert_eq!(outcome, Outcome::Shipped);
    }

    #[test]
    fn run_short_circuits_on_empty_acceptance() {
        let provider = MockProvider {
            canned_response: "SHOULD_NOT_BE_CALLED".to_string(),
        };
        let outcome = run(input(Some(vec![])), &provider);
        assert_eq!(outcome, Outcome::Shipped);
    }

    #[test]
    fn run_returns_shipped_when_all_passed() {
        let provider = MockProvider {
            canned_response:
                r#"{"verdicts":[{"ac":"x","verdict":"passed","evidence":"diff covers x"}]}"#
                    .to_string(),
        };
        let outcome = run(input(Some(vec!["x".to_string()])), &provider);
        assert_eq!(outcome, Outcome::Shipped);
    }

    #[test]
    fn run_returns_rework_with_findings_when_any_failed() {
        let provider = MockProvider {
            canned_response: r#"{"verdicts":[{"ac":"x","verdict":"passed","evidence":"ok"},{"ac":"y","verdict":"failed","evidence":"y is missing"}]}"#.to_string(),
        };
        let outcome = run(
            input(Some(vec!["x".to_string(), "y".to_string()])),
            &provider,
        );
        match outcome {
            Outcome::Rework { findings } => {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].severity, "blocker");
                assert_eq!(findings[0].category, "ac-violation");
                assert!(findings[0].message.contains("y"));
                assert!(findings[0].message.contains("y is missing"));
            }
            _ => panic!("expected Rework, got {:?}", outcome),
        }
    }

    #[test]
    fn run_short_circuits_on_invalid_json() {
        let provider = MockProvider {
            canned_response: "not json".to_string(),
        };
        let outcome = run(input(Some(vec!["x".to_string()])), &provider);
        assert_eq!(outcome, Outcome::Shipped);
    }

    #[test]
    fn run_strips_json_code_fence() {
        let provider = MockProvider {
            canned_response: "```json\n{\"verdicts\":[{\"ac\":\"x\",\"verdict\":\"passed\",\"evidence\":\"ok\"}]}\n```".to_string(),
        };
        let outcome = run(input(Some(vec!["x".to_string()])), &provider);
        assert_eq!(outcome, Outcome::Shipped);
    }

    #[test]
    fn run_treats_uncertain_as_passing() {
        let provider = MockProvider {
            canned_response: r#"{"verdicts":[{"ac":"x","verdict":"uncertain","evidence":"can't tell"},{"ac":"y","verdict":"uncertain","evidence":"ambiguous"}]}"#.to_string(),
        };
        let outcome = run(
            input(Some(vec!["x".to_string(), "y".to_string()])),
            &provider,
        );
        assert_eq!(outcome, Outcome::Shipped);
    }
}
