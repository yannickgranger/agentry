//! Tests for the shipper-runner pure helpers (EPIC #161 wave-bash port,
//! v2). The workspace's `arch-ban-inline-cfg-test-in-src.cypher` rule
//! (PR #295) forbids inline `#[cfg(test)] mod tests` blocks in `src/`,
//! so this is a separate test-crate file (mirrors
//! `ci_watcher_runner_test.rs`).
//!
//! Reviewer-claude v1 found one Blocker on credential leakage — the
//! `remote_url_does_not_contain_token`, `extraheader_argv_shape`, and
//! `tail_stderr_scrubs_token` cases below enforce the corrected
//! invariants so the regression cannot recur.

use agentry_role_runtime::shipper_runner::{
    build_pr_create_body, classify_pre_push_rebase, git_fetch_argv, git_push_argv,
    parse_pr_response, parse_shipper_payload, push_url_credential_free, scrub_token,
    split_target_repo, tail_stderr_scrubbed, PrCreateResponse, PrePushRebaseDecision,
    ShipperPayload,
};
use serde_json::json;

const FAKE_TOKEN: &str = "ghs_FAKE_TOKEN_abcdef0123456789";

#[test]
fn parse_shipper_payload_happy_path() {
    let bundle = json!({
        "brief": {
            "id": "brf_work_123_thing",
            "payload": {
                "target_repo": "yg/agentry",
                "base_branch": "develop",
                "pr_title": "feat: thing",
                "pr_body": "body text",
                "forge_host": "agency.lab:3000",
            },
        },
    });
    let p = parse_shipper_payload(&bundle);
    assert_eq!(
        p,
        ShipperPayload {
            brief_id: "brf_work_123_thing".into(),
            target_repo: "yg/agentry".into(),
            base_branch: "develop".into(),
            pr_title: "feat: thing".into(),
            pr_body: "body text".into(),
            forge_host: "agency.lab:3000".into(),
        }
    );
}

#[test]
fn parse_shipper_payload_uses_defaults() {
    let bundle = json!({
        "brief": {
            "id": "brf_x",
            "payload": {},
        },
    });
    let p = parse_shipper_payload(&bundle);
    assert_eq!(p.brief_id, "brf_x");
    assert_eq!(p.target_repo, "yg/agentry");
    assert_eq!(p.base_branch, "develop");
    assert_eq!(p.pr_title, "auto(brf_x)");
    assert_eq!(p.pr_body, "Agentry-produced PR. See brief trace stream.");
    assert_eq!(p.forge_host, "agency.lab:3000");
}

#[test]
fn parse_shipper_payload_missing_forge_host_falls_back_to_default() {
    // Verifies the bash `// "agency.lab:3000"` fall-through is preserved
    // — empty forge_host on the bundle does NOT crash; defaults apply.
    let bundle = json!({
        "brief": {
            "id": "brf_y",
            "payload": {
                "forge_host": "",
            },
        },
    });
    let p = parse_shipper_payload(&bundle);
    assert_eq!(p.forge_host, "agency.lab:3000");
}

#[test]
fn remote_url_does_not_contain_token() {
    // v1 BLOCKER regression test: the credential-free URL must never
    // embed `oauth2:TOKEN@` (or any other token-bearing form) so a git
    // push failure cannot echo the token in stderr.
    let url = push_url_credential_free("agency.lab:3000", "yg/agentry");
    assert_eq!(url, "https://agency.lab:3000/yg/agentry.git");
    assert!(
        !url.contains("oauth2:"),
        "remote URL must not contain `oauth2:` prefix: {url}"
    );
    assert!(
        !url.contains(FAKE_TOKEN),
        "remote URL must not contain the token: {url}"
    );
    assert!(
        !url.contains('@'),
        "remote URL must not contain `@` (no embedded user-info): {url}"
    );
}

#[test]
fn extraheader_argv_shape() {
    // The git argv vector must carry `-c http.extraheader=Authorization: token <T>`
    // entries (verbatim port of the bash heredoc's auth mechanism). The
    // URL argv entry must NOT contain `:TOKEN@` — the token lives in
    // argv via the extraheader, not in any URL.
    let url = push_url_credential_free("agency.lab:3000", "yg/agentry");
    let argv = git_push_argv(FAKE_TOKEN, &url, "auto/brf_test");

    let extraheader = format!("http.extraheader=Authorization: token {FAKE_TOKEN}");
    let mut found_extraheader = false;
    for window in argv.windows(2) {
        if window[0] == "-c" && window[1] == extraheader {
            found_extraheader = true;
        }
    }
    assert!(
        found_extraheader,
        "argv must contain `-c http.extraheader=Authorization: token <TOKEN>` pair: {argv:?}",
    );

    // URL argv entry: token MUST NOT be embedded.
    let url_entry = argv
        .iter()
        .find(|s| s.starts_with("https://"))
        .expect("argv must include the push URL");
    assert!(
        !url_entry.contains(&format!(":{FAKE_TOKEN}@")),
        "URL argv entry must not embed the token: {url_entry}",
    );
    assert!(
        !url_entry.contains(FAKE_TOKEN),
        "URL argv entry must not contain the token at all: {url_entry}",
    );

    // Refspec + force-with-lease shape (from brief).
    assert!(argv.iter().any(|s| s == "HEAD:auto/brf_test"));
    assert!(argv.iter().any(|s| s == "--force-with-lease"));
    assert!(argv.iter().any(|s| s == "push"));
}

#[test]
fn tail_stderr_scrubs_token() {
    // Belt-and-suspenders: even if a stderr capture somehow contains
    // the token (env-var echo, future code drift, third-party tool
    // output), it must be redacted before the bytes hit any emitted
    // event.
    let stderr = format!(
        "fatal: unable to access 'https://oauth2:{FAKE_TOKEN}@agency.lab:3000/yg/agentry.git/': SSL error",
    );
    let scrubbed = tail_stderr_scrubbed(stderr.as_bytes(), 4096, FAKE_TOKEN);
    assert!(
        !scrubbed.contains(FAKE_TOKEN),
        "scrubbed stderr must not contain the token: {scrubbed}",
    );
    assert!(
        scrubbed.contains("[REDACTED]"),
        "scrubbed stderr must contain the redaction marker: {scrubbed}",
    );
}

#[test]
fn scrub_token_no_op_when_token_empty() {
    assert_eq!(scrub_token("any text", ""), "any text");
}

#[test]
fn scrub_token_replaces_all_occurrences() {
    let s = format!("{FAKE_TOKEN} and again {FAKE_TOKEN}");
    let scrubbed = scrub_token(&s, FAKE_TOKEN);
    assert_eq!(scrubbed, "[REDACTED] and again [REDACTED]");
}

#[test]
fn split_target_repo_parses_owner_and_repo() {
    let (owner, repo) = split_target_repo("yg/agentry");
    assert_eq!(owner, "yg");
    assert_eq!(repo, "agentry");
}

#[test]
fn build_pr_create_body_shape() {
    let body = build_pr_create_body("title", "body", "auto/brf_x", "develop");
    assert_eq!(body["title"], json!("title"));
    assert_eq!(body["body"], json!("body"));
    assert_eq!(body["head"], json!("auto/brf_x"));
    assert_eq!(body["base"], json!("develop"));
}

#[test]
fn parse_pr_response_happy_path() {
    let resp = json!({
        "html_url": "https://agency.lab:3000/yg/agentry/pulls/42",
        "number": 42,
    });
    assert_eq!(
        parse_pr_response(&resp),
        Some(PrCreateResponse {
            pr_number: 42,
            pr_url: "https://agency.lab:3000/yg/agentry/pulls/42".into(),
        }),
    );
}

#[test]
fn parse_pr_response_returns_none_on_missing_html_url() {
    // Bash check: `[ -z "$pr_url" ] || [ "$pr_url" = "null" ]` → failed.
    let resp = json!({"number": 42});
    assert_eq!(parse_pr_response(&resp), None);
}

#[test]
fn parse_pr_response_returns_none_on_empty_html_url() {
    let resp = json!({"html_url": "", "number": 42});
    assert_eq!(parse_pr_response(&resp), None);
}

#[test]
fn parse_pr_response_defaults_number_to_zero() {
    let resp = json!({"html_url": "https://x/y/pulls/1"});
    let parsed = parse_pr_response(&resp).expect("html_url present");
    assert_eq!(parsed.pr_number, 0);
}

#[test]
fn pre_push_fetch_argv_uses_extraheader() {
    // Pre-push fetch uses the same `-c http.extraheader=Authorization: token <T>`
    // mechanism as push — token NEVER in the URL. Closes the same v1
    // BLOCKER class for the new fetch step.
    let url = push_url_credential_free("agency.lab:3000", "yg/agentry");
    let argv = git_fetch_argv(FAKE_TOKEN, &url, "develop");

    let extraheader = format!("http.extraheader=Authorization: token {FAKE_TOKEN}");
    let mut found_extraheader = false;
    for window in argv.windows(2) {
        if window[0] == "-c" && window[1] == extraheader {
            found_extraheader = true;
        }
    }
    assert!(
        found_extraheader,
        "fetch argv must contain `-c http.extraheader=Authorization: token <TOKEN>` pair: {argv:?}",
    );

    let url_entry = argv
        .iter()
        .find(|s| s.starts_with("https://"))
        .expect("argv must include the fetch URL");
    assert_eq!(url_entry, "https://agency.lab:3000/yg/agentry.git");
    assert!(
        !url_entry.contains(FAKE_TOKEN),
        "fetch URL argv entry must not contain the token: {url_entry}",
    );
    assert!(
        !url_entry.contains('@'),
        "fetch URL argv entry must not contain `@`: {url_entry}",
    );

    assert!(argv.iter().any(|s| s == "fetch"));
    assert!(argv.iter().any(|s| s == "develop"));
}

#[test]
fn pre_push_rebase_conflict_emits_done_failed() {
    // Non-zero rebase exit + porcelain output with unmerged paths must
    // route to AbortConflict — the runner translates that into
    // emit_done(Failed) and skips the push step. Test asserts the
    // classifier behaviour the runner depends on.
    let porcelain = "UU crates/agentry-role-runtime/src/bin/shipper_runner.rs\n";
    let decision = classify_pre_push_rebase(1, porcelain);
    assert_eq!(decision, PrePushRebaseDecision::AbortConflict);
    assert_ne!(decision, PrePushRebaseDecision::Proceed);
}

#[test]
fn pre_push_clean_rebase_proceeds_to_push() {
    // Rebase exit 0 with empty porcelain -> Proceed (push step is reached).
    let decision = classify_pre_push_rebase(0, "");
    assert_eq!(decision, PrePushRebaseDecision::Proceed);
}
