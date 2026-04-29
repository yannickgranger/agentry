//! coder-claude-runner — entrypoint half of the coder-claude-agentry role.
//!
//! Ports the entrypoint half of `CODER_CLAUDE_AGENTRY_SCRIPT` (~104 LoC bash)
//! under EPIC #161 Wave 1.2a. Reads the bundle, builds the verb-structured
//! prompt (with prior-rework banner if applicable), writes
//! `/tmp/brief_vars.sh` so the (still-bash) exitpoint can `source` shared
//! state, and streams claude via the lib `stream_claude` helper.
//!
//! The exitpoint half (`CODER_CLAUDE_AGENTRY_EXITPOINT` — cargo fmt,
//! quality-hygiene, acceptance eval, self-review claude call,
//! dead-pub-check, git commit) is **deferred to Wave 1.2b**. This runner
//! deliberately preserves the entrypoint/exitpoint cross-language IPC via
//! `/tmp/brief_vars.sh` so 1.2a and 1.2b ship as small, independently
//! reviewable PRs.
//!
//! ## DoneGuard departure (intentional)
//!
//! The other Rust runners (null-agent, ac-verifier-runner,
//! reviewer-claude-runner) start with `let _g = DoneGuard::new()` so the
//! daemon always sees a terminal `done` event. The coder-claude entrypoint
//! is structurally different: when it succeeds, the bash exitpoint runs
//! next and emits the terminal `done`. A `DoneGuard::drop` at the end of
//! `main` would emit `done failed` *before* the exitpoint runs — and the
//! spawner's read loop breaks on the first terminal event, so the
//! exitpoint's events would be silently dropped.
//!
//! Therefore this runner does NOT use DoneGuard. Failure paths emit
//! `done failed` explicitly and exit. Success path exits cleanly without
//! a done event, letting the exitpoint own terminal emission. Panics
//! (genuinely unexpected) escape as a non-zero exit; the spawner already
//! synthesises `Failed { reason: "agent exited without done event" }` for
//! that case (see `compute_verdict` in `orchestrator-runtime/spawner.rs`).
//!
//! Once Wave 1.2b ports the exitpoint to Rust, the merged binary can adopt
//! the standard DoneGuard pattern.
//!
//! ## Behaviour preserved verbatim
//!
//! - read startup bundle on stdin
//! - extract `brief.id`, `brief.payload.{target_repo,base_branch,
//!   issue_title,issue_body,acceptance,forge_host}`, `brief.topology.name`
//! - parse blocker findings out of `team_context.messages[].payload.findings[]`
//! - build a "REWORK iteration" banner block from blocker findings if
//!   `finding_count > 0` (prohibitions / requirements joined with `; `)
//! - require `GITEA_TOKEN` in env; missing → `done failed`
//! - `mkdir -p /root/.claude`, `cd /workspace`, set git config
//! - branch name `auto/<brief_id>` is informational; the workspace was
//!   already allocated by `daemon::workspace::allocate` and is on this
//!   branch when the container starts
//! - write `/tmp/brief_vars.sh` so the bash exitpoint can `source` it
//! - build the prompt verbatim (verb-structured task framing + ship
//!   self-check note + topology-aware constraints)
//! - emit `calling claude -p` event with `prompt_bytes`
//! - call `stream_claude(brief_id, ".coder", prompt)`
//! - emit `claude reply received` event with `bytes`
//! - exit 0 — exitpoint takes over

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{self, Command};

use agentry_role_runtime::{emit_done, emit_event, read_bundle_value, stream_claude, StreamErr};
use orchestrator_types::{DoneReason, EventVerdict};
use serde_json::{json, Value};

const WORKSPACE_DIR: &str = "/workspace";
const BRIEF_VARS_PATH: &str = "/tmp/brief_vars.sh";

fn main() {
    if let Err(err) = run() {
        emit_event(json!({
            "error": err.event_msg,
            "detail": err.detail,
        }));
        emit_done(
            EventVerdict::Failed,
            Some(DoneReason {
                cause: err.cause.into(),
                exit_code: None,
            }),
        );
        // exit 0 so the spawner's read loop has already captured the
        // terminal event before bash exitpoint runs (its events are
        // discarded; see DoneGuard departure note in module docs).
        process::exit(0);
    }
    // Success: don't emit done. Exitpoint runs next and emits the
    // terminal verdict. Exit 0 so the bash bootstrap exec's the exitpoint.
}

#[derive(Debug)]
struct RunErr {
    event_msg: &'static str,
    detail: String,
    cause: &'static str,
}

fn run() -> Result<(), RunErr> {
    let bundle = read_bundle_value().map_err(|e| RunErr {
        event_msg: "failed to parse startup bundle",
        detail: e.to_string(),
        cause: "bundle_parse_failed",
    })?;

    let brief_id = pointer_str(&bundle, "/brief/id").to_string();
    let target_repo = pointer_str_or(&bundle, "/brief/payload/target_repo", "yg/agentry");
    let _ = target_repo; // bash extracts it but never references it; keep for parity comment
    let base_branch = pointer_str_or(&bundle, "/brief/payload/base_branch", "develop");
    let issue_title = pointer_str(&bundle, "/brief/payload/issue_title").to_string();
    let issue_body = pointer_str(&bundle, "/brief/payload/issue_body").to_string();
    let acceptance = pointer_str_or(&bundle, "/brief/payload/acceptance", "true");
    let _forge_host = pointer_str_or(&bundle, "/brief/payload/forge_host", "agency.lab:3000");
    let topology_name = pointer_str(&bundle, "/brief/topology/name").to_string();

    let blocker_findings = collect_blocker_findings(&bundle);
    let finding_count = blocker_findings.len();
    let rework_banner = if finding_count > 0 {
        let banner = build_rework_banner(&blocker_findings);
        emit_event(json!({
            "msg": "rework iteration — injecting prior findings into prompt",
            "blocker_count": finding_count,
        }));
        banner
    } else {
        String::new()
    };

    if std::env::var("GITEA_TOKEN")
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        return Err(RunErr {
            event_msg: "GITEA_TOKEN not in env",
            detail: String::new(),
            cause: "gitea_token_missing",
        });
    }

    let _ = fs::create_dir_all("/root/.claude");

    // git config — global so all subsequent invocations see it. Failure
    // here is genuinely unexpected; surface as a hard failure.
    git_config_global(&[
        ("user.email", "coder-claude-agentry@agentry.lab"),
        ("user.name", "coder-claude-agentry"),
        ("http.sslVerify", "false"),
    ])
    .map_err(|e| RunErr {
        event_msg: "git config --global failed",
        detail: e,
        cause: "git_config_failed",
    })?;

    let branch = format!("auto/{brief_id}");
    emit_event(json!({
        "msg": "workspace worktree ready",
        "branch": branch,
    }));

    write_brief_vars(
        BRIEF_VARS_PATH,
        &[
            ("brief_id", &brief_id),
            ("base_branch", &base_branch),
            ("issue_title", &issue_title),
            ("issue_body", &issue_body),
            ("acceptance", &acceptance),
            ("branch", &branch),
            ("topology_name", &topology_name),
        ],
    )
    .map_err(|e| RunErr {
        event_msg: "failed to write /tmp/brief_vars.sh",
        detail: e,
        cause: "brief_vars_write_failed",
    })?;

    let prompt = build_coder_prompt(
        &base_branch,
        &branch,
        &rework_banner,
        &issue_title,
        &issue_body,
        &acceptance,
    );

    emit_event(json!({
        "msg": "calling claude -p",
        "prompt_bytes": prompt.len(),
    }));

    let reply = match stream_claude(&brief_id, ".coder", &prompt) {
        Ok(r) => r,
        Err(StreamErr::ClaudeFailed { exit_code, detail }) => {
            // Match bash entrypoint's stream_claude failure envelope:
            // `done failed; exit 0` — exitpoint events would be ignored,
            // but the bash bootstrap continues to invoke it. We exit with
            // explicit done failed via the run() Err path.
            emit_event(json!({
                "error": "claude -p failed",
                "exit_code": exit_code,
                "detail": detail,
            }));
            return Err(RunErr {
                event_msg: "claude -p failed",
                detail: format!("exit_code={exit_code}"),
                cause: "claude_failed",
            });
        }
        Err(StreamErr::TranscriptEmpty { path }) => {
            emit_event(json!({
                "error": "tee_or_transcript_write_failed",
                "transcript_path": path,
            }));
            return Err(RunErr {
                event_msg: "tee_or_transcript_write_failed",
                detail: path,
                cause: "transcript_empty",
            });
        }
    };

    emit_event(json!({
        "msg": "claude reply received",
        "bytes": reply.len(),
    }));

    Ok(())
}

fn pointer_str<'a>(bundle: &'a Value, ptr: &str) -> &'a str {
    bundle.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

fn pointer_str_or(bundle: &Value, ptr: &str, default: &str) -> String {
    let s = pointer_str(bundle, ptr);
    if s.is_empty() {
        default.to_string()
    } else {
        s.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PriorFinding {
    message: String,
    prohibitions: Vec<String>,
    requirements: Vec<String>,
}

/// Walk `team_context.messages[].payload.findings[]`, keeping only
/// `severity == "blocker"` entries. Mirrors the bash `jq -c` filter
/// verbatim. Empty / missing fields default to empty vectors.
fn collect_blocker_findings(bundle: &Value) -> Vec<PriorFinding> {
    let messages = bundle
        .pointer("/team_context/messages")
        .and_then(Value::as_array);
    let Some(messages) = messages else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in messages {
        let findings = m.pointer("/payload/findings").and_then(Value::as_array);
        let Some(findings) = findings else {
            continue;
        };
        for f in findings {
            if f.get("severity").and_then(Value::as_str) != Some("blocker") {
                continue;
            }
            let message = f
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let prohibitions = string_array_field(f, "prohibitions");
            let requirements = string_array_field(f, "requirements");
            out.push(PriorFinding {
                message,
                prohibitions,
                requirements,
            });
        }
    }
    out
}

fn string_array_field(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn build_rework_banner(findings: &[PriorFinding]) -> String {
    let mut feedback_block = String::new();
    for (i, f) in findings.iter().enumerate() {
        if i > 0 {
            feedback_block.push('\n');
        }
        feedback_block.push_str(&format!(
            "- BLOCKER: {}\n  Prohibitions: {}\n  Requirements: {}",
            f.message,
            f.prohibitions.join("; "),
            f.requirements.join("; "),
        ));
    }
    format!(
        "**This is a REWORK iteration.**\n\
         \n\
         A prior coder pass on this brief shipped a commit that is already on HEAD of this worktree. \
         The reviewer flagged the following BLOCKER findings against that commit. Read the existing \
         diff with `git diff ${{base_branch}}...HEAD`, identify the sites the findings name, and edit \
         those sites to satisfy each requirement and avoid each prohibition. Do NOT replan from \
         scratch and do NOT recreate files that already exist.\n\
         \n\
         --- Prior reviewer findings ---\n\
         {feedback_block}\n\
         --- End findings ---"
    )
}

fn git_config_global(pairs: &[(&str, &str)]) -> Result<(), String> {
    for (k, v) in pairs {
        let out = Command::new("git")
            .arg("config")
            .arg("--global")
            .arg(k)
            .arg(v)
            .current_dir(WORKSPACE_DIR)
            .output()
            .map_err(|e| format!("spawn git config {k}: {e}"))?;
        if !out.status.success() {
            let detail = String::from_utf8_lossy(&out.stderr);
            return Err(format!("git config {k}={v}: {detail}"));
        }
    }
    // The bash `cd /workspace` happens before git config — workspace must
    // already exist. Verify rather than relying on cwd of the current
    // process (which may differ).
    let dot_git = Path::new(WORKSPACE_DIR).join(".git");
    if !dot_git.is_dir() && !dot_git.is_file() {
        return Err(format!("{WORKSPACE_DIR} is not a git repo"));
    }
    Ok(())
}

/// Write `/tmp/brief_vars.sh` in a form the bash exitpoint can `source`.
/// Each entry becomes `export <key>=<single-quoted-value>`. Single-quote
/// wrapping is POSIX and tolerates newlines, dollar signs, backticks; the
/// only escaping needed is `'` → `'\\''` (close, escape, reopen).
fn write_brief_vars(path: &str, vars: &[(&str, &str)]) -> Result<(), String> {
    let mut content = String::from("#!/bin/bash\n");
    for (k, v) in vars {
        content.push_str(&format!("export {k}={}\n", sh_single_quote(v)));
    }
    let mut f = fs::File::create(path).map_err(|e| format!("create {path}: {e}"))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("write {path}: {e}"))?;
    Ok(())
}

/// POSIX-safe single-quote a string for inclusion in a shell script.
/// `'` characters are escaped via the standard close-quote / backslash-quote /
/// reopen-quote sequence: `'\''`. Other characters (including newlines,
/// dollar signs, backticks) need no escaping inside single quotes.
fn sh_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn build_coder_prompt(
    base_branch: &str,
    branch: &str,
    rework_banner: &str,
    issue_title: &str,
    issue_body: &str,
    acceptance: &str,
) -> String {
    // Verbatim from CODER_CLAUDE_AGENTRY_SCRIPT — the verb framing,
    // ship-binary self-check note, rework banner injection point,
    // task title/body, constraints (workspace-only, acceptance, no
    // commit/push, v1+ topology guidance) all preserved bit-for-bit.
    format!(
        "You are the coder role inside the agentry autonomous team, operating in the\n\
         container-local working directory /workspace. The repo is cloned at\n\
         branch \"{base_branch}\"; you are on a fresh branch \"{branch}\".\n\
         \n\
         Your task is described in verb-structured form below. Follow it literally:\n\
         each verb (CREATE / UPDATE / REPLACE / DELETE / MOVE) names a transformation\n\
         on a specific file:line target. Do NOT invent additional changes.\n\
         \n\
         # /usr/local/bin/ship — runs the brief.kind's validator pipeline against /workspace and prints a JSON report. Calling it is OPTIONAL in this brief; brief 6 makes it the only path to publication. Use it as a self-check before git commit if you want.\n\
         \n\
         {rework_banner}\n\
         \n\
         Task title: {issue_title}\n\
         \n\
         Task body:\n\
         {issue_body}\n\
         \n\
         Constraints:\n\
         - Operate only inside /workspace. Never touch files outside it.\n\
         - When you are done editing, the acceptance command below must pass. You\n  \
           may run it yourself to check. The orchestrator will re-run it before\n  \
           accepting the diff:\n    \
           {acceptance}\n\
         - Do not commit or push. The orchestrator handles commit and push on your\n  \
           behalf after you exit.\n\
         - The orchestrator may be running you in `agentry-self-host-v1` topology (or a later v1+). In that case: do not commit, do not push. The `/usr/local/bin/ship` tool (when called) runs the validator pipeline against your changes; if it returns ok, exit and the orchestrator's git-operator role takes over. Topology name is in $topology_name.\n\
         \n\
         When the transformations are complete and the acceptance passes, simply\n\
         report success and exit.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pointer_str_or_uses_default_when_missing() {
        let v = json!({"a": "value"});
        assert_eq!(pointer_str_or(&v, "/missing", "default"), "default");
        assert_eq!(pointer_str_or(&v, "/a", "default"), "value");
    }

    #[test]
    fn pointer_str_or_uses_default_when_empty() {
        let v = json!({"a": ""});
        assert_eq!(pointer_str_or(&v, "/a", "default"), "default");
    }

    #[test]
    fn collect_blocker_findings_filters_by_severity() {
        let bundle = json!({
            "team_context": {
                "messages": [
                    {
                        "payload": {
                            "findings": [
                                {
                                    "severity": "blocker",
                                    "message": "wrong abstraction",
                                    "prohibitions": ["do not refactor"],
                                    "requirements": ["preserve api"]
                                },
                                {
                                    "severity": "warn",
                                    "message": "minor style"
                                }
                            ]
                        }
                    }
                ]
            }
        });
        let v = collect_blocker_findings(&bundle);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].message, "wrong abstraction");
        assert_eq!(v[0].prohibitions, vec!["do not refactor"]);
        assert_eq!(v[0].requirements, vec!["preserve api"]);
    }

    #[test]
    fn collect_blocker_findings_handles_missing_team_context() {
        let bundle = json!({});
        assert_eq!(collect_blocker_findings(&bundle).len(), 0);
    }

    #[test]
    fn collect_blocker_findings_handles_message_without_findings() {
        let bundle = json!({"team_context": {"messages": [{"payload": {"other": 1}}]}});
        assert_eq!(collect_blocker_findings(&bundle).len(), 0);
    }

    #[test]
    fn collect_blocker_findings_drops_findings_without_severity() {
        let bundle = json!({
            "team_context": {
                "messages": [
                    {"payload": {"findings": [{"message": "no severity field"}]}}
                ]
            }
        });
        assert_eq!(collect_blocker_findings(&bundle).len(), 0);
    }

    #[test]
    fn build_rework_banner_joins_constraints_with_semicolons() {
        let f = vec![PriorFinding {
            message: "bad change".into(),
            prohibitions: vec!["a".into(), "b".into()],
            requirements: vec!["c".into()],
        }];
        let banner = build_rework_banner(&f);
        assert!(banner.contains("**This is a REWORK iteration.**"));
        assert!(banner.contains("- BLOCKER: bad change"));
        assert!(banner.contains("Prohibitions: a; b"));
        assert!(banner.contains("Requirements: c"));
        assert!(banner.contains("--- End findings ---"));
    }

    #[test]
    fn build_rework_banner_handles_multiple_findings() {
        let f = vec![
            PriorFinding {
                message: "first".into(),
                prohibitions: vec![],
                requirements: vec![],
            },
            PriorFinding {
                message: "second".into(),
                prohibitions: vec![],
                requirements: vec![],
            },
        ];
        let banner = build_rework_banner(&f);
        assert!(banner.contains("- BLOCKER: first"));
        assert!(banner.contains("- BLOCKER: second"));
    }

    #[test]
    fn build_rework_banner_keeps_dollar_brace_base_branch_literal() {
        // The bash heredoc keeps `${base_branch}` literal in the prompt
        // text — claude reads the literal string as guidance to use the
        // bash variable name. The Rust port must NOT interpolate it.
        let banner = build_rework_banner(&[]);
        assert!(
            banner.contains("git diff ${base_branch}...HEAD"),
            "banner must keep `${{base_branch}}` literal: {banner}"
        );
    }

    #[test]
    fn sh_single_quote_wraps_simple_value() {
        assert_eq!(sh_single_quote("value"), "'value'");
        assert_eq!(sh_single_quote(""), "''");
    }

    #[test]
    fn sh_single_quote_escapes_embedded_single_quote() {
        // POSIX recipe: '\\''  (close, escape, reopen).
        assert_eq!(sh_single_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_single_quote("''"), "''\\'''\\'''");
    }

    #[test]
    fn sh_single_quote_passes_through_specials() {
        // Inside single quotes, $, `, \\, !, * are all literal.
        assert_eq!(sh_single_quote("$VAR"), "'$VAR'");
        assert_eq!(sh_single_quote("`cmd`"), "'`cmd`'");
        assert_eq!(sh_single_quote("\\n"), "'\\n'");
        assert_eq!(sh_single_quote("multi\nline"), "'multi\nline'");
    }

    #[test]
    fn write_brief_vars_emits_sourceable_script() {
        let dir = std::env::temp_dir().join("agentry_coder_runner_brief_vars_test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("brief_vars.sh");
        let path_s = path.to_str().expect("tempdir path is utf8");
        write_brief_vars(
            path_s,
            &[
                ("brief_id", "brf_test_123"),
                ("issue_body", "line1\nline2 with $var and 'quote'"),
            ],
        )
        .expect("write should succeed");
        let contents = fs::read_to_string(&path).expect("read written file");
        assert!(contents.starts_with("#!/bin/bash\n"));
        assert!(contents.contains("export brief_id='brf_test_123'\n"));
        // Single-quoted, with embedded ' replaced by '\\''
        assert!(
            contents.contains("export issue_body='line1\nline2 with $var and '\\''quote'\\'''\n"),
            "single-quote escape mismatch: {contents}"
        );
    }

    #[test]
    fn build_coder_prompt_contains_required_anchors() {
        let p = build_coder_prompt(
            "develop",
            "auto/brf_x",
            "",
            "Fix bug",
            "CREATE foo.rs:10",
            "cargo test",
        );
        assert!(p.contains("branch \"develop\""));
        assert!(p.contains("branch \"auto/brf_x\""));
        assert!(p.contains("Task title: Fix bug"));
        assert!(p.contains("CREATE foo.rs:10"));
        assert!(p.contains("cargo test"));
        assert!(p.contains("agentry-self-host-v1"));
    }

    #[test]
    fn build_coder_prompt_injects_rework_banner_when_present() {
        let banner = "**This is a REWORK iteration.**\n[banner body]";
        let p = build_coder_prompt("develop", "auto/brf_x", banner, "T", "B", "true");
        assert!(p.contains("**This is a REWORK iteration.**"));
        assert!(p.contains("[banner body]"));
    }

    #[test]
    fn build_coder_prompt_omits_rework_banner_when_empty() {
        let p = build_coder_prompt("develop", "auto/brf_x", "", "T", "B", "true");
        assert!(!p.contains("**This is a REWORK iteration.**"));
    }
}
