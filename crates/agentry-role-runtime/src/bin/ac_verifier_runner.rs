//! ac-verifier-runner — workspace-prep + provider-invocation wrapper.
//!
//! Ports the three `AC_VERIFIER_{CLAUDE,GEMINI,GROK}_AGENTRY_SCRIPT` bash
//! entrypoints to one Rust binary parameterized by `--provider`. The bash
//! versions were ~70 lines each, 99 % identical. EPIC #161 Wave 1.3.
//!
//! Behaviour preserved verbatim from bash:
//!
//! - read startup bundle on stdin
//! - extract `permit.agent_id`, `brief.payload.{base_branch,issue_body,acceptance_criteria}`
//! - empty/null AC list → emit_event "skipping" + emit_done shipped (fast path)
//! - workspace not a git repo → emit_event error + emit_done shipped
//! - `git fetch origin <base_branch>` failure → emit_event "git fetch failed" + emit_done shipped
//! - `git diff origin/<base_branch>..HEAD` failure → emit_event "git diff failed" + emit_done shipped
//! - provider binary not on PATH → emit_event "ac_verifier_unavailable" + emit_done shipped
//! - `timeout $CLAUDE_P_TIMEOUT <binary> <<<bundle>` invocation failure → emit_event
//!   "<provider> invocation failed" + emit_done shipped
//! - outcome.outcome == "rework" → emit one finding per `outcome.findings[i]` then
//!   emit_done rework_needed
//! - otherwise → emit_done shipped
//!
//! Every degradation path emits `done shipped` (NOT failed) on purpose: the
//! bash kept reviewer-claude as the architectural backstop, and a `failed`
//! verdict here would short-circuit the topology before the reviewer runs.
//!
//! `DoneGuard` covers the residual path where the binary unwinds without
//! reaching one of the explicit `emit_done` calls (panic, abrupt return) —
//! the daemon always sees a terminal `done` event. EPIC #161 B0 invariant.

use std::io::{self, Write};
use std::process::{Command, Stdio};

use agentry_role_runtime::{
    emit_done, emit_event, emit_finding, parse_severity, read_bundle_value, tail_bytes, tail_lines,
    workspace_is_git_repo, DoneGuard,
};
use orchestrator_types::{EventVerdict, FindingOrigin, ReviewFinding};
use serde_json::{json, Value};

/// Which AC verifier provider this invocation runs.
///
/// Each variant maps 1:1 to a bind-mounted host binary on `PATH` inside the
/// container. The string returned by `binary_name` is used both for `Command`
/// dispatch and for human-readable msg prefixes in degradation events
/// (preserving the bash scripts' wording).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Claude,
    Gemini,
    Grok,
}

impl Provider {
    fn binary_name(self) -> &'static str {
        match self {
            Provider::Claude => "ac-verifier",
            Provider::Gemini => "ac-verifier-gemini",
            Provider::Grok => "ac-verifier-grok",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Provider::Claude),
            "gemini" => Some(Provider::Gemini),
            "grok" => Some(Provider::Grok),
            _ => None,
        }
    }
}

fn main() {
    let _guard = DoneGuard::new();

    let provider = match parse_provider_arg() {
        Ok(p) => p,
        Err(msg) => {
            emit_event(json!({
                "error": "ac-verifier-runner: bad --provider flag",
                "detail": msg,
            }));
            // Match bash degradation envelope: shipped, not failed.
            emit_done(EventVerdict::Shipped, None);
            return;
        }
    };

    let bundle = match read_bundle_value() {
        Ok(v) => v,
        Err(e) => {
            emit_event(json!({
                "error": "failed to parse startup bundle",
                "detail": e.to_string(),
            }));
            emit_done(EventVerdict::Shipped, None);
            return;
        }
    };

    let agent_id = bundle
        .pointer("/permit/agent_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let base_branch = bundle
        .pointer("/brief/payload/base_branch")
        .and_then(Value::as_str)
        .unwrap_or("develop")
        .to_string();
    let verb_body = bundle
        .pointer("/brief/payload/issue_body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let acs_value = bundle
        .pointer("/brief/payload/acceptance_criteria")
        .cloned()
        .unwrap_or(Value::Null);

    // Fast path: empty/null acceptance_criteria → skip without spending tokens.
    let acs_array = match &acs_value {
        Value::Array(arr) if !arr.is_empty() => arr.clone(),
        _ => {
            emit_event(json!({
                "msg": format!(
                    "no acceptance_criteria in payload — skipping {}",
                    provider.binary_name(),
                ),
            }));
            emit_done(EventVerdict::Shipped, None);
            return;
        }
    };

    if !workspace_is_git_repo("/workspace") {
        emit_event(json!({
            "error": "workspace is not a git repo — coder did not produce it",
        }));
        emit_done(EventVerdict::Shipped, None);
        return;
    }

    if let Err(err) = git_fetch_origin(&base_branch) {
        emit_event(json!({
            "msg": "git fetch failed — degrading to shipped",
            "detail": err,
        }));
        emit_done(EventVerdict::Shipped, None);
        return;
    }

    let diff_text = match git_diff_against(&base_branch) {
        Ok(d) => d,
        Err(err) => {
            emit_event(json!({
                "msg": "git diff failed — degrading to shipped",
                "detail": err,
            }));
            emit_done(EventVerdict::Shipped, None);
            return;
        }
    };

    let inner = json!({
        "acceptance_criteria": acs_array,
        "diff": diff_text,
        "verb_body": verb_body,
    });
    let inner_json = serde_json::to_string(&inner).unwrap_or_else(|_| String::from("{}"));

    let outcome_text = match invoke_provider(provider, &inner_json) {
        Ok(out) => out,
        Err(InvokeErr::NotFound) => {
            emit_event(json!({
                "warn": "ac_verifier_unavailable",
                "detail": format!(
                    "{} binary not on PATH; reviewer-claude is the backstop",
                    provider.binary_name(),
                ),
            }));
            emit_done(EventVerdict::Shipped, None);
            return;
        }
        Err(InvokeErr::Failed {
            exit_code,
            stderr_tail,
        }) => {
            emit_event(json!({
                "msg": format!(
                    "{} invocation failed — degrading to shipped",
                    provider.binary_name(),
                ),
                "exit_code": exit_code,
                "detail": stderr_tail,
            }));
            emit_done(EventVerdict::Shipped, None);
            return;
        }
    };

    dispatch_outcome(provider, &agent_id, &outcome_text);
}

fn parse_provider_arg() -> Result<Provider, String> {
    let mut args = std::env::args().skip(1);
    let mut provider: Option<Provider> = None;

    while let Some(a) = args.next() {
        if a == "--provider" {
            let v = args
                .next()
                .ok_or_else(|| "--provider requires a value".to_string())?;
            provider =
                Some(Provider::parse(&v).ok_or_else(|| {
                    format!("unknown provider '{v}' (expected claude|gemini|grok)")
                })?);
        } else if let Some(rest) = a.strip_prefix("--provider=") {
            provider = Some(Provider::parse(rest).ok_or_else(|| {
                format!("unknown provider '{rest}' (expected claude|gemini|grok)")
            })?);
        } else {
            return Err(format!("unknown argument '{a}'"));
        }
    }

    provider.ok_or_else(|| "--provider <claude|gemini|grok> is required".to_string())
}

fn git_fetch_origin(base_branch: &str) -> Result<(), String> {
    let out = Command::new("git")
        .arg("fetch")
        .arg("origin")
        .arg(base_branch)
        .current_dir("/workspace")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git fetch: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // Bash captured stdout+stderr together via `>/tmp/acv_fetch.err 2>&1`,
    // then `tail -20`. Combine streams and tail the last 20 lines to match.
    let mut combined = Vec::with_capacity(out.stdout.len() + out.stderr.len());
    combined.extend_from_slice(&out.stdout);
    combined.extend_from_slice(&out.stderr);
    Err(tail_lines(&combined, 20))
}

fn git_diff_against(base_branch: &str) -> Result<String, String> {
    let range = format!("origin/{base_branch}..HEAD");
    let out = Command::new("git")
        .arg("diff")
        .arg(&range)
        .current_dir("/workspace")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn git diff: {e}"))?;
    if !out.status.success() {
        return Err(tail_lines(&out.stderr, 20));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

enum InvokeErr {
    NotFound,
    Failed { exit_code: i32, stderr_tail: String },
}

fn invoke_provider(provider: Provider, inner_json: &str) -> Result<String, InvokeErr> {
    // Mirror bash exactly: `timeout "$CLAUDE_P_TIMEOUT" <binary> <<<json`.
    // Spawning the host `timeout(1)` keeps wall-clock semantics identical
    // (exit 124 on TLE) without pulling in a Rust async timeout dep.
    let timeout_secs = std::env::var("CLAUDE_P_TIMEOUT").unwrap_or_else(|_| "1200".into());
    let mut cmd = Command::new("timeout");
    cmd.arg(&timeout_secs)
        .arg(provider.binary_name())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // `timeout` itself missing is fatal; treat the same as the binary
            // missing — both are "ac-verifier infra not available". The bash
            // distinguished between binary-missing (warn) and binary-failed
            // (exit_code) but never had to consider missing `timeout`
            // (coreutils ships it). Surface as NotFound to stay conservative.
            return Err(InvokeErr::NotFound);
        }
        Err(e) => {
            return Err(InvokeErr::Failed {
                exit_code: -1,
                stderr_tail: format!("spawn timeout(1): {e}"),
            });
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(inner_json.as_bytes()) {
            return Err(InvokeErr::Failed {
                exit_code: -1,
                stderr_tail: format!("write stdin: {e}"),
            });
        }
    }

    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return Err(InvokeErr::Failed {
                exit_code: -1,
                stderr_tail: format!("wait child: {e}"),
            });
        }
    };

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }

    // exit 127 from `timeout` = command-not-found. Map to NotFound so the
    // emit matches the bash `command -v` pre-flight branch.
    let code = out.status.code().unwrap_or(-1);
    if code == 127 {
        return Err(InvokeErr::NotFound);
    }
    let tail = tail_bytes(&out.stderr, 2048);
    Err(InvokeErr::Failed {
        exit_code: code,
        stderr_tail: tail,
    })
}

fn dispatch_outcome(provider: Provider, agent_id: &str, outcome_text: &str) {
    let parsed: Value = serde_json::from_str(outcome_text).unwrap_or(Value::Null);
    let outcome_str = parsed
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("shipped");

    if outcome_str != "rework" {
        emit_event(json!({
            "msg": format!(
                "{} shipped — all acceptance criteria met or unverifiable",
                provider.binary_name(),
            ),
        }));
        emit_done(EventVerdict::Shipped, None);
        return;
    }

    let findings = parsed
        .get("findings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    emit_event(json!({
        "msg": format!("{} rework", provider.binary_name()),
        "findings_count": findings.len(),
    }));

    for f in &findings {
        let severity = parse_severity(f.get("severity").and_then(Value::as_str));
        let category = f
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let message = f
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        emit_finding(&ReviewFinding {
            file: None,
            line: None,
            severity,
            origin: FindingOrigin::Model {
                reviewer_agent_id: agent_id.to_string(),
            },
            category,
            message,
            suggested_fix: None,
            prohibitions: Vec::new(),
            requirements: Vec::new(),
        });
    }

    emit_done(EventVerdict::ReworkNeeded, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parse_known_values() {
        assert_eq!(Provider::parse("claude"), Some(Provider::Claude));
        assert_eq!(Provider::parse("gemini"), Some(Provider::Gemini));
        assert_eq!(Provider::parse("grok"), Some(Provider::Grok));
    }

    #[test]
    fn provider_parse_unknown_returns_none() {
        assert_eq!(Provider::parse("openai"), None);
        assert_eq!(Provider::parse(""), None);
    }

    #[test]
    fn provider_binary_name_matches_bash_command_v_target() {
        // These exact strings appear in the bash `command -v` checks of the
        // three AC_VERIFIER_*_AGENTRY_SCRIPT consts — keeping the Rust port
        // wire-compatible with the host bind-mounts.
        assert_eq!(Provider::Claude.binary_name(), "ac-verifier");
        assert_eq!(Provider::Gemini.binary_name(), "ac-verifier-gemini");
        assert_eq!(Provider::Grok.binary_name(), "ac-verifier-grok");
    }
}
