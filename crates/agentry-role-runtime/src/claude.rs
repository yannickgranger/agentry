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

use crate::{emit_event, emit_tool_refused};

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
    stream_claude_inner(brief_id, suffix, prompt, false)
}

/// Variant for single-turn, large-prompt callers (reviewer-claude).
/// Writes the prompt to a temp file and uses `Stdio::from(File)` so
/// claude reads stdin from the file descriptor instead of receiving
/// the prompt as a CLI positional. Avoids E2BIG on large diffs
/// without breaking multi-turn agentic-loop semantics that the
/// positional path preserves for the coder runner.
pub fn stream_claude_via_stdin(
    brief_id: &str,
    suffix: &str,
    prompt: &str,
) -> Result<String, StreamErr> {
    stream_claude_inner(brief_id, suffix, prompt, true)
}

fn stream_claude_inner(
    brief_id: &str,
    suffix: &str,
    prompt: &str,
    via_stdin: bool,
) -> Result<String, StreamErr> {
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

    let timeout_secs = std::env::var("CLAUDE_P_TIMEOUT").unwrap_or_else(|_| "3600".into());

    // If via_stdin: write the prompt to a temp file and feed claude's
    // stdin from that file. Big diffs (#495 beta-b reviewer prompt
    // ~241KB) push ARG_MAX on positional, so file-backed stdin is the
    // escape hatch. File-backed stdin (Stdio::from(File)) matches
    // shell `claude -p < file.txt` semantics — the OS handles the
    // close on full read, no premature EOF on a pipe buffer.
    //
    // Positional is preserved for multi-turn agentic-loop callers
    // (coder-claude-runner): with positional, claude treats the arg
    // as "the whole task, work autonomously"; with stdin, claude
    // appears to read once and exit the agentic loop earlier
    // (observed empirically in beta-a v5/v6).
    let prompt_file_path = if via_stdin {
        // Use a stable suffix-derived path so it appears in the
        // brief's transcript layout, but stays small enough to
        // tolerate prompts under /tmp's tmpfs quota.
        let path = format!("/tmp/claude-prompt-{brief_id}{suffix}.txt");
        if let Err(e) = fs::write(&path, prompt) {
            return Err(StreamErr::ClaudeFailed {
                exit_code: -1,
                detail: format!("write prompt file {path}: {e}"),
            });
        }
        Some(path)
    } else {
        None
    };

    let mut cmd = Command::new("timeout");
    cmd.arg(&timeout_secs)
        .arg("claude")
        .arg("-p")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose");
    if !via_stdin {
        cmd.arg(prompt);
    }
    cmd.env("HOME", "/root");

    if let Some(ref path) = prompt_file_path {
        match fs::File::open(path) {
            Ok(file) => {
                cmd.stdin(Stdio::from(file));
            }
            Err(e) => {
                return Err(StreamErr::ClaudeFailed {
                    exit_code: -1,
                    detail: format!("open prompt file {path}: {e}"),
                });
            }
        }
    } else {
        cmd.stdin(Stdio::null());
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

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
        if let Some((tool, command)) = parse_tool_refusal(&line) {
            emit_tool_refused(&tool, command.as_deref());
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

/// JSON-strict parse of a single `claude -p --output-format stream-json`
/// transcript line for a tool-refusal signal. Returns `Some((tool,
/// command))` only when the line is a JSON object that carries a
/// canonical refusal shape:
///
/// - top-level `"type":"tool_use_denied"`, OR
/// - top-level `"permission_denied":true`
///
/// `tool` is taken from the top-level `"tool"` field; `command` from the
/// top-level `"command"` field when present (e.g. the Bash command line),
/// `None` otherwise. Returns `None` for non-JSON, non-object, or
/// ambiguous lines — substring-only matches do NOT count.
fn parse_tool_refusal(line: &str) -> Option<(String, Option<String>)> {
    let v: Value = serde_json::from_str(line).ok()?;
    if !v.is_object() {
        return None;
    }
    let is_refusal = v.get("type").and_then(Value::as_str) == Some("tool_use_denied")
        || v.get("permission_denied").and_then(Value::as_bool) == Some(true);
    if !is_refusal {
        return None;
    }
    let tool = v.get("tool").and_then(Value::as_str)?.to_string();
    let command = v.get("command").and_then(Value::as_str).map(str::to_string);
    Some((tool, command))
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
