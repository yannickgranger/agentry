//! AC-verifier binary (Gemini provider variant). Reads
//! `{acceptance_criteria, diff, verb_body}` JSON on stdin, calls into
//! `ac_verifier::run` with `GeminiProvider`, and emits a single JSON outcome
//! line on stdout. The role's bash script wraps this invocation in
//! `timeout $CLAUDE_P_TIMEOUT` and parses the outcome line.

use std::io::{self, Read};

use coder_precommit::ac_verifier::{self, Input, Outcome};
use coder_precommit::providers::GeminiProvider;

fn main() {
    let mut buf = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut buf) {
        eprintln!("ac-verifier-gemini: failed to read stdin: {e}");
        std::process::exit(1);
    }
    let input: Input = match serde_json::from_str(&buf) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("ac-verifier-gemini: malformed stdin JSON: {e}");
            std::process::exit(1);
        }
    };
    let provider = GeminiProvider::default();
    let outcome = ac_verifier::run(input, &provider);
    match outcome {
        Outcome::Shipped => {
            println!("{}", serde_json::json!({"outcome": "shipped"}));
        }
        Outcome::Rework { findings } => {
            let findings_json: Vec<serde_json::Value> = findings
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "severity": f.severity,
                        "category": f.category,
                        "message": f.message,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({"outcome": "rework", "findings": findings_json})
            );
        }
    }
}
