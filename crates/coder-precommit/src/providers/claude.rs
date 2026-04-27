//! `claude -p` provider for ac-verifier. Shells out to the host claude CLI
//! mounted into the container at /usr/local/bin/claude. The role's bash script
//! wraps the binary call in `timeout $CLAUDE_P_TIMEOUT`, so no timeout here.

use std::io;
use std::process::Command;

use super::AcVerifierProvider;

pub struct ClaudeProvider;

impl AcVerifierProvider for ClaudeProvider {
    fn verify(&self, system: &str, user: &str) -> io::Result<String> {
        let prompt = format!("{system}\n\n---\n\n{user}");
        let output = Command::new("claude")
            .args(["-p", "--output-format", "text", &prompt])
            .env("HOME", "/root")
            .output()?;
        if !output.status.success() {
            let stderr_string = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(io::Error::other(stderr_string));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
