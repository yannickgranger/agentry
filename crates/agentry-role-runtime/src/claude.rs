//! `stream_claude` — invoke `claude -p`, mirror each stream-json line as a
//! structured trace event, tee to a transcript file, return the assistant's
//! final text. Lifted from `reviewer_claude_runner.rs` (EPIC #161 Wave 1.4)
//! when a second consumer (coder-claude-runner, Wave 1.2a) appeared.
//!
//! Wire-compatible with the bash `stream_claude` helper in `BASH_PRELUDE`:
//! same env (`HOME=/root`), same coreutils-`timeout(1)` wrapper, same
//! event shape (`{claude:<obj>}` for valid-JSON lines, `{claude_raw:<str>}`
//! for malformed ones), same transcript layout.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::emit_event;

const TRANSCRIPTS_DIR: &str = "/transcripts";

/// Failure modes for [`stream_claude`].
///
/// Both variants have the same caller obligation: emit a degradation event
/// and `done failed`. The split exists so callers can distinguish "claude
/// reported failure" (exit code in stderr) from "claude reported success
/// but the transcript is empty" (rootless podman subuid mismatch on
/// `/transcripts` — bash hit this regularly enough to warrant defence in
/// depth).
#[derive(Debug)]
pub enum StreamErr {
    /// `timeout(1) claude -p ...` exited non-zero, or the spawn itself
    /// failed. `exit_code` is `-1` for spawn / wait errors, otherwise the
    /// child's exit code (124 = timeout, 127 = command-not-found, etc.).
    /// `detail` is the tail of stderr (≤4 KiB).
    ClaudeFailed { exit_code: i32, detail: String },
    /// Child reported success, but the transcript file is missing or
    /// zero-byte. Almost always means tee/transcript bind-mount permissions
    /// rejected the write — surface explicitly so the operator sees the
    /// real failure mode rather than a downstream parse error.
    TranscriptEmpty { path: String },
}

/// Spawn `timeout $CLAUDE_P_TIMEOUT claude -p --output-format stream-json
/// --verbose <prompt>`, mirror each stdout line as a structured event AND
/// append it to `/transcripts/<brief_id><suffix>.jsonl`. After the child
/// exits, parse the transcript for the assistant's final text.
///
/// `suffix` lets multiple claude calls within one brief co-exist
/// (e.g. `.coder` for the entrypoint, `.self-review` for an exitpoint
/// soft-fail call, `.reviewer` for the reviewer role).
///
/// Mirrors the bash `BASH_PRELUDE::stream_claude` helper bit-for-bit:
/// same env (`HOME=/root`), same `timeout(1)` wrapper, same wire shape,
/// same transcript layout. Used by reviewer-claude-runner and
/// coder-claude-runner (and any future role binary that needs a streamed
/// claude call).
///
/// On success, returns the assistant's final text. The reconstruction
/// prefers the `result`-typed line's `.result` field; falls back to the
/// concatenation of `assistant.message.content[].text` segments if no
/// `result` line is present.
pub fn stream_claude(brief_id: &str, suffix: &str, prompt: &str) -> Result<String, StreamErr> {
    let _ = fs::create_dir_all(TRANSCRIPTS_DIR);
    let transcript_path = format!("{TRANSCRIPTS_DIR}/{brief_id}{suffix}.jsonl");

    let mut transcript = match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&transcript_path)
    {
        Ok(f) => f,
        Err(e) => {
            return Err(StreamErr::ClaudeFailed {
                exit_code: -1,
                detail: format!("open transcript {transcript_path}: {e}"),
            });
        }
    };

    let timeout_secs = std::env::var("CLAUDE_P_TIMEOUT").unwrap_or_else(|_| "1200".into());

    // bash: `HOME=/root timeout "$CLAUDE_P_TIMEOUT" claude -p --output-format stream-json --verbose "$_prompt" 2>&1`
    let mut cmd = Command::new("timeout");
    cmd.arg(&timeout_secs)
        .arg("claude")
        .arg("-p")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg(prompt)
        .env("HOME", "/root")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Bash redirected 2>&1 so stderr ends up in the same stream the
        // tee/while-read pipeline consumes. Mirror by merging stderr into
        // a sibling-thread drain so an emit-event consumer can see it.
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(StreamErr::ClaudeFailed {
                exit_code: -1,
                detail: format!("spawn timeout(1) claude -p: {e}"),
            });
        }
    };

    let stdout = child
        .stdout
        .take()
        .expect("piped stdout not connected to child");
    let stderr = child.stderr.take();

    let stderr_handle = stderr.map(|s| {
        std::thread::spawn(move || {
            let mut tail = Vec::new();
            let mut buf = [0u8; 4096];
            let mut s = s;
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => tail.extend_from_slice(&buf[..n]),
                }
            }
            // Keep only the last 4 KiB so a chatty claude can't blow up
            // memory; bash's eventual `tail -c 4096` semantics on err.
            if tail.len() > 4096 {
                let cut = tail.len() - 4096;
                tail.drain(..cut);
            }
            String::from_utf8_lossy(&tail).into_owned()
        })
    });

    let reader = BufReader::new(stdout);
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => break,
        };
        if writeln!(transcript, "{line}").is_err() {
            // Transcript write failure is detected post-wait via the
            // empty-file guard below — bash's defence-in-depth.
        }
        emit_claude_line(&line);
    }
    let _ = transcript.flush();

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            return Err(StreamErr::ClaudeFailed {
                exit_code: -1,
                detail: format!("wait child: {e}"),
            });
        }
    };
    let stderr_tail = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let exit_code = status.code().unwrap_or(-1);
    if !status.success() {
        return Err(StreamErr::ClaudeFailed {
            exit_code,
            detail: stderr_tail,
        });
    }

    let meta = fs::metadata(&transcript_path).map_err(|e| StreamErr::ClaudeFailed {
        exit_code,
        detail: format!("stat transcript {transcript_path}: {e}"),
    })?;
    if meta.len() == 0 {
        return Err(StreamErr::TranscriptEmpty {
            path: transcript_path,
        });
    }

    Ok(reconstruct_assistant_text(&transcript_path))
}

fn emit_claude_line(line: &str) {
    if let Ok(parsed) = serde_json::from_str::<Value>(line) {
        emit_event(json!({"claude": parsed}));
    } else {
        emit_event(json!({"claude_raw": line}));
    }
}

/// Walk the transcript, prefer the `result` event's `.result` field; if
/// missing, concatenate `assistant.message.content[].text` segments. Bash
/// behaviour was identical (and the latter fallback was added in PR #129
/// to avoid `tail -1` truncating multi-line JSON).
fn reconstruct_assistant_text(transcript_path: &str) -> String {
    let f = match fs::File::open(transcript_path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let reader = BufReader::new(f);
    let mut result_field: Option<String> = None;
    let mut assistant_chunks: Vec<String> = Vec::new();
    for line in reader.lines().map_while(io::Result::ok) {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(Value::as_str) {
            Some("result") => {
                if let Some(r) = v.get("result").and_then(Value::as_str) {
                    result_field = Some(r.to_string());
                }
            }
            Some("assistant") => {
                let content = v.pointer("/message/content").and_then(Value::as_array);
                if let Some(arr) = content {
                    for c in arr {
                        if c.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(t) = c.get("text").and_then(Value::as_str) {
                                assistant_chunks.push(t.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    result_field.unwrap_or_else(|| assistant_chunks.join(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstruct_assistant_text_prefers_result_field() {
        let dir = std::env::temp_dir().join("agentry_role_runtime_claude_recon_test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("test.jsonl");
        let _ = fs::write(
            &path,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"draft\"}]}}\n\
             {\"type\":\"result\",\"result\":\"final\"}\n",
        );
        let s = reconstruct_assistant_text(path.to_str().expect("tempdir path is utf8"));
        assert_eq!(s, "final");
    }

    #[test]
    fn reconstruct_assistant_text_falls_back_to_assistant_chunks() {
        let dir = std::env::temp_dir().join("agentry_role_runtime_claude_recon_test_2");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("test.jsonl");
        let _ = fs::write(
            &path,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"part1\"}]}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"part2\"}]}}\n",
        );
        let s = reconstruct_assistant_text(path.to_str().expect("tempdir path is utf8"));
        assert_eq!(s, "part1part2");
    }

    #[test]
    fn reconstruct_assistant_text_returns_empty_for_missing_file() {
        let path = "/tmp/nonexistent-transcript-bcdef98765.jsonl";
        let _ = fs::remove_file(path);
        assert_eq!(reconstruct_assistant_text(path), "");
    }
}
