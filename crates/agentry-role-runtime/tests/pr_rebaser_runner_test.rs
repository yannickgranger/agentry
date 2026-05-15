//! Tests for the pr-rebaser-runner pure helpers (EPIC #161 wave-bash port).
//! The workspace's `arch-ban-inline-cfg-test-in-src.cypher` rule (PR #295)
//! forbids inline `#[cfg(test)] mod tests` blocks in `src/`, so this is a
//! separate test-crate file (mirrors `ci_watcher_runner_test.rs`).

use agentry_role_runtime::pr_rebaser::{
    classify_rebase, compose_remote_url, parse_rebaser_payload, parse_unmerged_files,
    push_force_with_lease_args, PayloadError, RebaseOutcome, RebaserPayload,
};
use serde_json::json;

#[test]
fn parse_rebaser_payload_happy_path() {
    let bundle = json!({
        "brief": {
            "payload": {
                "target_repo": "yg/agentry",
                "pr_number": 42,
                "branch": "auto/brf_work_1_foo",
                "base_branch": "develop",
                "forge_host": "forge.example.com:3000",
            }
        }
    });
    let p = parse_rebaser_payload(&bundle).expect("payload parses");
    assert_eq!(
        p,
        RebaserPayload {
            target_repo: "yg/agentry".into(),
            pr_number: 42,
            branch: "auto/brf_work_1_foo".into(),
            base_branch: "develop".into(),
            forge_host: "forge.example.com:3000".into(),
        }
    );
}

#[test]
fn parse_rebaser_payload_applies_defaults() {
    // Only the branch is required; everything else falls back to a sane
    // default mirroring the bash `// "..."` shape.
    let bundle = json!({
        "brief": {
            "payload": {
                "branch": "auto/feature-x",
            }
        }
    });
    let p = parse_rebaser_payload(&bundle).expect("payload parses with defaults");
    assert_eq!(p.target_repo, "yg/agentry");
    assert_eq!(p.pr_number, 0);
    assert_eq!(p.branch, "auto/feature-x");
    assert_eq!(p.base_branch, "develop");
    assert_eq!(p.forge_host, "forge.example.com:3000");
}

#[test]
fn parse_rebaser_payload_pr_number_string_form() {
    // Bash `jq -r` returns the value as a string; the runner accepts both
    // numeric and string forms so a forge that quotes IDs still parses.
    let bundle = json!({
        "brief": {
            "payload": {
                "branch": "auto/x",
                "pr_number": "137",
            }
        }
    });
    let p = parse_rebaser_payload(&bundle).expect("payload parses");
    assert_eq!(p.pr_number, 137);
}

#[test]
fn parse_rebaser_payload_missing_branch_is_error() {
    let bundle = json!({
        "brief": {
            "payload": {
                "target_repo": "yg/agentry",
                "pr_number": 7,
            }
        }
    });
    assert_eq!(
        parse_rebaser_payload(&bundle),
        Err(PayloadError::MissingBranch)
    );
}

#[test]
fn parse_rebaser_payload_empty_branch_is_error() {
    let bundle = json!({
        "brief": {
            "payload": {
                "branch": "",
            }
        }
    });
    assert_eq!(
        parse_rebaser_payload(&bundle),
        Err(PayloadError::MissingBranch)
    );
}

#[test]
fn compose_remote_url_shape() {
    let url = compose_remote_url("forge.example.com:3000", "yg/agentry", "abc123");
    assert_eq!(
        url,
        "https://oauth2:abc123@forge.example.com:3000/yg/agentry.git",
    );
}

#[test]
fn compose_remote_url_handles_alt_forge_host() {
    let url = compose_remote_url("forge.example.com", "owner/repo", "T0KEN");
    assert_eq!(url, "https://oauth2:T0KEN@forge.example.com/owner/repo.git");
}

#[test]
fn parse_unmerged_files_porcelain_v2_conflicts() {
    // Porcelain v2 conflict line shape:
    //   u <X><Y> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
    let status = "\
1 .M N... 100644 100644 100644 abc def src/clean.rs\n\
u UU N... 100644 100644 100644 100644 aaa bbb ccc crates/foo/src/lib.rs\n\
u UU N... 100644 100644 100644 100644 ddd eee fff crates/bar/Cargo.toml\n";
    let files = parse_unmerged_files(status);
    assert_eq!(
        files,
        vec![
            "crates/foo/src/lib.rs".to_string(),
            "crates/bar/Cargo.toml".to_string(),
        ]
    );
}

#[test]
fn parse_unmerged_files_empty_status() {
    assert!(parse_unmerged_files("").is_empty());
}

#[test]
fn parse_unmerged_files_no_conflicts() {
    let status = "\
1 .M N... 100644 100644 100644 abc def src/clean.rs\n\
1 M. N... 100644 100644 100644 abc def src/staged.rs\n";
    assert!(parse_unmerged_files(status).is_empty());
}

#[test]
fn push_force_with_lease_args_shape() {
    assert_eq!(
        push_force_with_lease_args("auto/x"),
        vec![
            "push".to_string(),
            "--force-with-lease".to_string(),
            "origin".to_string(),
            "auto/x".to_string(),
        ]
    );
}

#[test]
fn classify_rebase_success_on_zero_exit() {
    assert_eq!(classify_rebase(0, ""), RebaseOutcome::Success);
    // Even with a non-empty status, a 0 exit means the rebase succeeded —
    // unrelated dirt in the worktree (shouldn't happen on a fresh clone but
    // defensive) does not flip the outcome.
    assert_eq!(
        classify_rebase(0, "u UU N... 100644 100644 100644 100644 a b c f.rs\n"),
        RebaseOutcome::Success,
    );
}

#[test]
fn classify_rebase_conflict_on_nonzero_with_unmerged() {
    let status = "u UU N... 100644 100644 100644 100644 a b c crates/foo/lib.rs\n";
    assert_eq!(classify_rebase(1, status), RebaseOutcome::Conflict);
    assert_eq!(classify_rebase(128, status), RebaseOutcome::Conflict);
}

#[test]
fn classify_rebase_fatal_on_nonzero_without_unmerged() {
    // Non-zero rebase exit but porcelain v2 reports no conflict entries —
    // detached HEAD, missing ref, etc. The runner must NOT classify this
    // as a conflict (which would request human review) — it's a substrate
    // failure that should fall through to `done failed`.
    assert_eq!(classify_rebase(1, ""), RebaseOutcome::Fatal);
    assert_eq!(
        classify_rebase(128, "1 .M N... 100644 100644 100644 abc def src/clean.rs\n",),
        RebaseOutcome::Fatal,
    );
}
