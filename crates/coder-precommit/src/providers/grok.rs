//! Grok (xAI) provider for ac-verifier. Shells out via `curl` to POST to
//! `https://api.x.ai/v1/chat/completions`. The role's bash script wraps the
//! binary call in `timeout $CLAUDE_P_TIMEOUT`, so no timeout here.
//!
//! `XAI_API_KEY` is read from our process env and passed to the curl child
//! via env (`Command::env`); the JSON request body is piped to curl on stdin
//! (`-d @-`). Neither value appears in curl's argv, so the API key cannot
//! leak through `/proc/<pid>/cmdline` or `ps`.

use std::env;
use std::io::{self, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use super::AcVerifierProvider;

pub struct GrokProvider {
    pub model: String,
}

impl Default for GrokProvider {
    fn default() -> Self {
        Self {
            model: "grok-4-fast".into(),
        }
    }
}

impl AcVerifierProvider for GrokProvider {
    fn verify(&self, system: &str, user: &str) -> io::Result<String> {
        let api_key = env::var("XAI_API_KEY")
            .map_err(|_| io::Error::other("XAI_API_KEY not set in environment"))?;

        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "max_tokens": 4096,
        })
        .to_string();

        let mut child = Command::new("curl")
            .args([
                "-sS",
                "-f",
                "-X",
                "POST",
                "https://api.x.ai/v1/chat/completions",
                "-H",
                "Content-Type: application/json",
                "-H",
                "@-",
                "-d",
                &body,
            ])
            .env("XAI_API_KEY", &api_key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "Authorization: Bearer {api_key}")?;
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr_string = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(io::Error::other(format!(
                "curl to xAI failed: {stderr_string}"
            )));
        }

        let resp: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| io::Error::other(format!("xAI response was not valid JSON: {e}")))?;

        let content = resp
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| io::Error::other("xAI response missing .choices[0].message.content"))?;

        Ok(content.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_default_is_grok_4_fast() {
        let p = GrokProvider::default();
        assert_eq!(p.model, "grok-4-fast");
    }
}
