use coder_precommit::ac_verifier::{run, Input, Outcome};
use coder_precommit::providers::AcVerifierProvider;

struct MockProvider {
    canned_response: String,
}

impl AcVerifierProvider for MockProvider {
    fn verify(&self, _system: &str, _user: &str) -> std::io::Result<String> {
        Ok(self.canned_response.clone())
    }
}

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
            r#"{"verdicts":[{"ac":"x","verdict":"passed","evidence":"diff covers x"}]}"#.to_string(),
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
