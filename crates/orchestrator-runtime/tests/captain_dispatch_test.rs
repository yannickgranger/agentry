//! Integration tests for `captain dispatch`.
//!
//! Tests run the compiled `captain` binary as a subprocess and write
//! temporary brief files via `tempfile`. Tests use `--dry-run` so the live
//! `orchestrator submit` path is not exercised here — that path is exercised
//! at acceptance time when this brief itself is dispatched.

use orchestrator_runtime::captain_dispatch_env::populate_env_from_disk;
use orchestrator_types::{
    now, Assertion, AssertionAnchor, AssertionId, Brief, BriefId, Budget, Contract, EscalationMode,
    TaskShape, VersionedRef,
};
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::sync::Mutex;
use tempfile::NamedTempFile;

/// Serialises the in-process env-mutating tests below. Subprocess tests
/// don't share env with the parent so they don't need this guard; the
/// `populate_env_from_disk` tests do, because they read $HOME and write
/// AGENTRY_* in the parent process.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII helper: snapshot a fixed set of env vars, clear them, restore on
/// drop. Lets each populate_env_from_disk test start from a known-empty
/// state and leaves the process env unchanged for subsequent tests.
struct EnvSnapshot {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvSnapshot {
    fn capture(keys: &[&'static str]) -> Self {
        let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in keys {
            std::env::remove_var(k);
        }
        Self { saved }
    }
}

impl Drop for EnvSnapshot {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}

const TRACKED_ENV: &[&str] = &[
    "HOME",
    "AGENTRY_REDIS_PASSWORD",
    "AGENTRY_REDIS__URL",
    "AGENTRY_SIGNING__KEY_PATH",
];

fn captain_bin() -> &'static str {
    env!("CARGO_BIN_EXE_captain")
}

fn write_temp_brief(json: Value) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("create tempfile");
    let text = serde_json::to_string_pretty(&json).expect("serialize brief json");
    f.write_all(text.as_bytes()).expect("write brief json");
    f.flush().expect("flush brief json");
    f
}

fn build_brief(kind: Option<TaskShape>, contract: Option<Contract>) -> Brief {
    Brief {
        id: BriefId("brf_test_dispatch".into()),
        project: None,
        topology: VersionedRef::new("agentry-bugfix-v0", 1),
        payload: serde_json::json!({
            "issue_title": "test",
            "issue_body": "test",
            "acceptance": "true",
            "target_repo": "yg/agentry",
            "base_branch": "develop",
            "pr_title": "test",
            "pr_body": "test",
        }),
        kind,
        contract,
        budget: Budget::default(),
        escalation: EscalationMode::Autonomous,
        parent_brief: None,
        cohort_labels: Vec::new(),
        redeploy_required: vec![],
        submitted_by: "captain-cli-test".to_string(),
        submitted_at: now(),
    }
}

fn run_dispatch(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(captain_bin());
    // Supply the three env vars `captain dispatch` now resolves up-front so
    // the disk-probe fallback (`populate_env_from_disk`) is a no-op for
    // these integration tests. Without this, the tests would depend on the
    // host having `~/.config/agentry/{redis.password,signing.key}` set up,
    // which is not portable to fresh CI containers.
    cmd.env("AGENTRY_REDIS_PASSWORD", "test-redis-password")
        .env("AGENTRY_REDIS__URL", "redis://:test@127.0.0.1:6380")
        .env("AGENTRY_SIGNING__KEY_PATH", "/tmp/test-signing.key");
    cmd.arg("dispatch");
    cmd.args(args);
    cmd.output().expect("spawn captain dispatch")
}

#[test]
fn captain_dispatch_dry_run_validates_minimal_trivial_doc() {
    let brief = build_brief(Some(TaskShape::TrivialDoc), None);
    let json = serde_json::to_value(&brief).expect("brief to value");
    let f = write_temp_brief(json);
    let path = f.path().to_str().expect("tempfile path utf-8");
    let out = run_dispatch(&["--dry-run", path]);
    assert!(
        out.status.success(),
        "captain dispatch --dry-run should succeed; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("validated"),
        "expected `validated` in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&brief.id.0),
        "expected brief id `{}` in stderr:\n{stderr}",
        brief.id.0
    );
    assert!(
        stderr.contains("kind=Some(TrivialDoc)"),
        "expected `kind=Some(TrivialDoc)` in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("contract_present=false"),
        "expected `contract_present=false` in stderr:\n{stderr}"
    );
}

#[test]
fn captain_dispatch_dry_run_succeeds_on_feature_with_contract() {
    let brief_id = BriefId("brf_test_feature_contract".into());
    let contract = Contract {
        brief_id: brief_id.clone(),
        assertions: vec![Assertion {
            id: AssertionId("A1".into()),
            prose: "feature must compile".into(),
            anchor: AssertionAnchor::Cfdb {
                qname: "crate::feature::foo".into(),
            },
        }],
        precursor_artifacts: Vec::new(),
    };
    let mut brief = build_brief(Some(TaskShape::Feature), Some(contract));
    brief.id = brief_id;
    let json = serde_json::to_value(&brief).expect("brief to value");
    let f = write_temp_brief(json);
    let path = f.path().to_str().expect("tempfile path utf-8");
    let out = run_dispatch(&["--dry-run", path]);
    assert!(
        out.status.success(),
        "captain dispatch --dry-run should succeed; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("contract_present=true"),
        "expected `contract_present=true` in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("assertions=1"),
        "expected `assertions=1` in stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("WARN"),
        "expected no WARN line for feature-with-contract:\n{stderr}"
    );
}

#[test]
fn captain_dispatch_dry_run_warns_on_feature_without_contract() {
    let brief = build_brief(Some(TaskShape::Feature), None);
    let json = serde_json::to_value(&brief).expect("brief to value");
    let f = write_temp_brief(json);
    let path = f.path().to_str().expect("tempfile path utf-8");
    let out = run_dispatch(&["--dry-run", path]);
    assert!(
        out.status.success(),
        "captain dispatch --dry-run should succeed even with WARN; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WARN"),
        "expected `WARN` in stderr for feature-without-contract:\n{stderr}"
    );
    assert!(
        stderr.contains("requires contract"),
        "expected `requires contract` in stderr:\n{stderr}"
    );
}

#[test]
fn captain_dispatch_rejects_invalid_brief_json() {
    let mut f = NamedTempFile::new().expect("create tempfile");
    f.write_all(b"{ this is not valid json")
        .expect("write malformed json");
    f.flush().expect("flush malformed json");
    let path = f.path().to_str().expect("tempfile path utf-8");
    let out = run_dispatch(&["--dry-run", path]);
    assert!(
        !out.status.success(),
        "expected non-zero exit for malformed brief; status={:?} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parse") || stderr.contains("Brief"),
        "expected stderr to describe a parse error; got:\n{stderr}"
    );
}

#[test]
fn populate_env_from_disk_reads_redis_password_and_constructs_url() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _env = EnvSnapshot::capture(TRACKED_ENV);

    let home = tempfile::tempdir().expect("create tempdir for HOME");
    let cfg = home.path().join(".config").join("agentry");
    std::fs::create_dir_all(&cfg).expect("create config dir");
    std::fs::write(cfg.join("redis.password"), "s3cret-pw\n").expect("write redis.password");
    std::fs::write(cfg.join("signing.key"), b"key-bytes").expect("write signing.key");
    std::env::set_var("HOME", home.path());

    populate_env_from_disk().expect("populate_env_from_disk should succeed");

    assert_eq!(
        std::env::var("AGENTRY_REDIS_PASSWORD").ok().as_deref(),
        Some("s3cret-pw"),
        "redis password should be loaded from disk and trimmed"
    );
    assert_eq!(
        std::env::var("AGENTRY_REDIS__URL").ok().as_deref(),
        Some("redis://:s3cret-pw@127.0.0.1:6380"),
        "redis url should be derived from the loaded password"
    );
    let signing = std::env::var("AGENTRY_SIGNING__KEY_PATH").expect("signing key path set");
    assert_eq!(
        std::path::PathBuf::from(signing),
        cfg.join("signing.key"),
        "signing key path should default under $HOME/.config/agentry/"
    );
}

#[test]
fn populate_env_from_disk_reports_all_three_when_config_dir_missing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _env = EnvSnapshot::capture(TRACKED_ENV);

    let home = tempfile::tempdir().expect("create tempdir for HOME");
    std::env::set_var("HOME", home.path());

    let err = populate_env_from_disk()
        .expect_err("populate_env_from_disk should fail with no config dir");
    let msg = format!("{err:#}");

    assert!(
        msg.contains("3 missing configuration"),
        "expected three missing items in error:\n{msg}"
    );
}

#[test]
fn populate_env_from_disk_reports_only_signing_key_when_password_present() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _env = EnvSnapshot::capture(TRACKED_ENV);

    let home = tempfile::tempdir().expect("create tempdir for HOME");
    let cfg = home.path().join(".config").join("agentry");
    std::fs::create_dir_all(&cfg).expect("create config dir");
    std::fs::write(cfg.join("redis.password"), "pw\n").expect("write redis.password");
    std::env::set_var("HOME", home.path());

    let err = populate_env_from_disk()
        .expect_err("populate_env_from_disk should fail without signing.key");
    let msg = format!("{err:#}");

    assert!(
        msg.contains("1 missing configuration"),
        "expected exactly one missing item:\n{msg}"
    );
    assert_eq!(
        std::env::var("AGENTRY_REDIS_PASSWORD").ok().as_deref(),
        Some("pw"),
        "redis password should still be resolved from disk"
    );
    assert!(
        std::env::var("AGENTRY_REDIS__URL").is_ok(),
        "redis url should be derived from the resolved password"
    );
    let signing = std::env::var("AGENTRY_SIGNING__KEY_PATH").expect("signing key path set");
    assert_eq!(
        std::path::PathBuf::from(signing),
        cfg.join("signing.key"),
        "signing key env should default to canonical path even when file is missing"
    );
}

#[test]
fn captain_dispatch_help_lists_dry_run_flag() {
    let out = Command::new(captain_bin())
        .args(["dispatch", "--help"])
        .output()
        .expect("spawn captain dispatch --help");
    assert!(
        out.status.success(),
        "captain dispatch --help should succeed; status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--dry-run"),
        "expected `--dry-run` in help output:\n{stdout}"
    );
}
