//! Tests for `run_fence` (brief Y.3) — smoke + threshold-folding.

use agentry_role_runtime::{
    clones_to_findings, complexity_to_findings, run_fence, unwraps_to_findings,
};
use orchestrator_types::review::{FindingOrigin, Severity};
use serde_json::json;
use std::path::Path;

#[test]
fn run_fence_pipeline_does_not_panic() {
    // Smoke: drives the real pipeline against /workspace. The test
    // environment may or may not have `origin/develop` available; both
    // outcomes (empty Vec, Vec with entries) are acceptable. The contract
    // we verify here is "returns Vec<ReviewFinding> without panicking".
    let v = run_fence(Path::new("/workspace"), "develop");
    let _len = v.len();
}

#[test]
fn clones_emits_in_loop_finding() {
    let json = json!({
        "functions": [{
            "name": "hot",
            "line": 42,
            "clone_calls": 1,
            "clones_in_loop": 1,
            "arc_rc_pattern": 0,
        }],
    });
    let v = clones_to_findings("src/foo.rs", &json);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].file.as_deref(), Some("src/foo.rs"));
    assert_eq!(v[0].line, Some(42));
    assert_eq!(v[0].severity, Severity::Blocker);
    match &v[0].origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, "ra-query");
            assert_eq!(rule.as_deref(), Some("clones_in_loop"));
        }
        other => panic!("expected Mechanical origin, got {other:?}"),
    }
}

#[test]
fn clones_emits_clone_prod_when_arc_rc_does_not_account_for_all() {
    let json = json!({
        "functions": [{
            "name": "noisy",
            "line": 10,
            "clone_calls": 3,
            "clones_in_loop": 0,
            "arc_rc_pattern": 1,
        }],
    });
    let v = clones_to_findings("src/foo.rs", &json);
    assert_eq!(v.len(), 1);
    match &v[0].origin {
        FindingOrigin::Mechanical { rule, .. } => {
            assert_eq!(rule.as_deref(), Some("clone_prod"));
        }
        other => panic!("expected Mechanical origin, got {other:?}"),
    }
}

#[test]
fn clones_emits_nothing_when_all_arc_rc() {
    let json = json!({
        "functions": [{
            "name": "ok",
            "line": 5,
            "clone_calls": 2,
            "clones_in_loop": 0,
            "arc_rc_pattern": 2,
        }],
    });
    assert!(clones_to_findings("src/foo.rs", &json).is_empty());
}

#[test]
fn complexity_emits_one_per_function() {
    let json = json!({
        "functions": [
            { "name": "a", "line": 1, "cognitive": 16 },
            { "name": "b", "line": 30, "cognitive": 25 },
        ],
    });
    let v = complexity_to_findings("src/foo.rs", &json);
    assert_eq!(v.len(), 2);
    for f in &v {
        assert_eq!(f.severity, Severity::Blocker);
        match &f.origin {
            FindingOrigin::Mechanical { rule, tool } => {
                assert_eq!(rule.as_deref(), Some("complexity"));
                assert_eq!(tool, "ra-query");
            }
            other => panic!("expected Mechanical origin, got {other:?}"),
        }
    }
}

#[test]
fn unwraps_emits_one_per_function_with_line() {
    let json = json!({
        "functions": [
            { "name": "f", "line": 8, "total": 2 },
        ],
    });
    let v = unwraps_to_findings("src/foo.rs", &json);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].line, Some(8));
    assert_eq!(v[0].file.as_deref(), Some("src/foo.rs"));
    match &v[0].origin {
        FindingOrigin::Mechanical { rule, .. } => {
            assert_eq!(rule.as_deref(), Some("unwraps"));
        }
        other => panic!("expected Mechanical origin, got {other:?}"),
    }
}

#[test]
fn empty_functions_yields_empty_findings() {
    let empty = json!({"functions": []});
    assert!(clones_to_findings("x.rs", &empty).is_empty());
    assert!(complexity_to_findings("x.rs", &empty).is_empty());
    assert!(unwraps_to_findings("x.rs", &empty).is_empty());
}

#[test]
fn missing_functions_field_is_handled() {
    let v = json!({"summary": {}});
    assert!(clones_to_findings("x.rs", &v).is_empty());
    assert!(complexity_to_findings("x.rs", &v).is_empty());
    assert!(unwraps_to_findings("x.rs", &v).is_empty());
}

// ---------- callers fence (Y.4) — synthetic git-repo integration ----------

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn unique_tempdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "agentry-fence-test-{tag}-{}-{nanos}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

fn run(cmd: &mut Command) {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "{:?} failed: {}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git(dir: &Path, args: &[&str]) {
    run(Command::new("git").args(args).current_dir(dir));
}

fn init_synthetic_repo(initial_lib: &str) -> PathBuf {
    let bare = unique_tempdir("bare");
    run(Command::new("git").args(["init", "--bare"]).arg(&bare));

    let work = unique_tempdir("work");
    git(&work, &["init", "-b", "develop"]);
    git(&work, &["config", "user.email", "test@example.com"]);
    git(&work, &["config", "user.name", "Test"]);
    let bare_url = bare.to_string_lossy().into_owned();
    git(&work, &["remote", "add", "origin", &bare_url]);

    std::fs::create_dir_all(work.join("src")).expect("mkdir src");
    std::fs::write(
        work.join("Cargo.toml"),
        "[package]\nname = \"synth\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .expect("write toml");
    std::fs::write(work.join("src/lib.rs"), initial_lib).expect("write lib");

    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "base"]);
    git(&work, &["push", "-u", "origin", "develop"]);

    git(&work, &["checkout", "-b", "feature"]);
    work
}

/// run_fence must not panic when the workspace lacks `origin/<base>` — the
/// pre-diff worktree creation surfaces as a Blocker finding instead. This
/// is the one stable behaviour we can assert without ra-query backing it.
#[test]
fn callers_fence_emits_finding_when_pre_diff_worktree_unavailable() {
    let scratch = unique_tempdir("noorigin");
    git(&scratch, &["init", "-b", "develop"]);
    git(&scratch, &["config", "user.email", "test@example.com"]);
    git(&scratch, &["config", "user.name", "Test"]);
    std::fs::write(scratch.join("README"), "x").expect("write");
    git(&scratch, &["add", "."]);
    git(&scratch, &["commit", "-m", "init"]);

    // No origin remote → worktree add origin/develop must fail. The fence
    // turns that into an ra_query_unavailable / git-worktree finding.
    let v = run_fence(&scratch, "develop");
    assert!(
        v.iter().any(|f| matches!(
            &f.origin,
            FindingOrigin::Mechanical { rule, .. } if rule.as_deref() == Some("ra_query_unavailable")
        )),
        "expected ra_query_unavailable finding, got {v:?}"
    );
}

/// Diff that modifies an existing pub fn (no addition) must not synthesise
/// a `callers_zero` finding, regardless of whether ra-query callers
/// resolves anything. Verifies the difference()-by-(name,kind,file) shape:
/// line/col drift on an unchanged symbol does not count as a new pub item.
#[test]
fn callers_fence_does_not_fire_on_pub_fn_modification() {
    let work = init_synthetic_repo("pub fn unchanged() -> i32 { 1 }\n");
    // Modify in place — same pub item, different body line offset.
    std::fs::write(
        work.join("src/lib.rs"),
        "// added comment shifts the body\npub fn unchanged() -> i32 { 2 }\n",
    )
    .expect("write");
    git(&work, &["commit", "-am", "modify body"]);

    let v = run_fence(&work, "develop");
    assert!(
        !v.iter().any(|f| matches!(
            &f.origin,
            FindingOrigin::Mechanical { rule, .. } if rule.as_deref() == Some("callers_zero")
        )),
        "no callers_zero finding expected for pure-modification diff, got {v:?}"
    );
}

/// Diff that does NOT touch any `*.rs` file outside tests/ has no changed
/// files → callers fence has nothing to check → run_fence returns no
/// callers_zero / callers_unresolved findings.
#[test]
fn callers_fence_silent_on_empty_rust_diff() {
    let work = init_synthetic_repo("pub fn already() {}\n");
    // Add only a non-Rust file.
    std::fs::write(work.join("notes.md"), "hello").expect("write");
    git(&work, &["add", "notes.md"]);
    git(&work, &["commit", "-m", "docs"]);

    let v = run_fence(&work, "develop");
    let callers_findings: Vec<_> = v
        .iter()
        .filter(|f| {
            matches!(
                &f.origin,
                FindingOrigin::Mechanical { rule, .. }
                    if matches!(rule.as_deref(), Some("callers_zero") | Some("callers_unresolved"))
            )
        })
        .collect();
    assert!(
        callers_findings.is_empty(),
        "no callers fence findings expected on non-Rust diff, got {callers_findings:?}"
    );
}

/// Regression for v3 reviewer blocker: a pure-body modification of an
/// existing pub fn (3 pre-existing pub items kept, 0 added) must NEVER
/// produce callers_zero blockers — even if pub-surface resolution fails
/// on the pre worktree side. The v2 cascade bug emitted N false-positive
/// callers_zero blockers on pre-existing items when the pre vec came
/// back silently empty; v3 short-circuits to a single
/// `pub_surface_unresolved` meta-finding instead.
#[test]
fn callers_fence_no_cascade_on_pure_body_modification() {
    let initial = "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n";
    let work = init_synthetic_repo(initial);
    // Modify only the body of `beta`; alpha/gamma untouched as pub items.
    std::fs::write(
        work.join("src/lib.rs"),
        "pub fn alpha() {}\npub fn beta() -> i32 { 7 }\npub fn gamma() {}\n",
    )
    .expect("write");
    git(&work, &["commit", "-am", "modify beta body"]);

    let v = run_fence(&work, "develop");
    let zero_count = v
        .iter()
        .filter(|f| {
            matches!(
                &f.origin,
                FindingOrigin::Mechanical { rule, .. }
                    if rule.as_deref() == Some("callers_zero")
            )
        })
        .count();
    assert_eq!(
        zero_count, 0,
        "pre-existing pub items must never produce callers_zero blockers; got {v:?}"
    );
}
