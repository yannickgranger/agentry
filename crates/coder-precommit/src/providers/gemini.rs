//! Gemini `generateContent` provider for ac-verifier. Shells out to `curl` and
//! POSTs to `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent`.
//! The role's bash script wraps the binary call in `timeout $CLAUDE_P_TIMEOUT`,
//! so no timeout here. The `GEMINI_API_KEY` is passed to the curl child via env
//! (not as a literal argv entry from the Rust caller) so the key doesn't appear
//! in the parent process's command line.

use std::io;
use std::io::Write;
use std::process::{Command, Stdio};

use super::AcVerifierProvider;

pub struct GeminiProvider {
    pub model: String,
}

impl Default for GeminiProvider {
    fn default() -> Self {
        Self {
            model: "gemini-3-flash-preview".into(),
        }
    }
}

impl AcVerifierProvider for GeminiProvider {
    fn verify(&self, system: &str, user: &str) -> io::Result<String> {
        let api_key = std::env::var("GEMINI_API_KEY")
            .map_err(|_| io::Error::other("GEMINI_API_KEY not set"))?;
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );
        let body = serde_json::json!({
            "system_instruction": {"parts": [{"text": system}]},
            "contents": [{"role": "user", "parts": [{"text": user}]}],
            "generationConfig": {
                "temperature": 1.0,
                "responseMimeType": "application/json",
            }
        })
        .to_string();
        // Shell out via bash so the `x-goog-api-key` header expands from
        // GEMINI_API_KEY in the curl child's environment — the key is set
        // only via Command::env, never as a Rust-level argv entry.
        let script = "exec curl -sS -X POST \
            -H \"x-goog-api-key: ${GEMINI_API_KEY}\" \
            -H 'Content-Type: application/json' \
            --data-binary @- \
            \"$1\"";
        let mut child = Command::new("bash")
            .args(["-c", script, "bash", &url])
            .env("GEMINI_API_KEY", &api_key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("failed to open curl stdin"))?
            .write_all(body.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr_string = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(io::Error::other(format!("curl failed: {stderr_string}")));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let v: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|e| io::Error::other(format!("gemini response not JSON: {e}")))?;
        let text = v
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.get(0))
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "gemini response missing .candidates[0].content.parts[0].text: {stdout}"
                ))
            })?;
        Ok(text.to_string())
    }
}
