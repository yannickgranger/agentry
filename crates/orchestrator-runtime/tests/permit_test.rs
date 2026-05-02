//! Integration tests for the permit broker (sign / verify / load).

use ed25519_dalek::SigningKey;
use orchestrator_runtime::permit::{load_signing_key, sign, tool_allowed, verify};
use orchestrator_runtime::Error;
use orchestrator_types::{now, BriefId, PermitScope, RoleName, ToolAllowlist, WorkPermit};
use rand_core::OsRng;

fn sample_permit() -> WorkPermit {
    WorkPermit {
        permit_id: "prm_t".into(),
        agent_id: "agt_t".into(),
        role: RoleName("t".into()),
        brief: BriefId("brf_t".into()),
        tool_allowlist: ToolAllowlist(vec!["read".into()]),
        allowed_tools: None,
        permit_scope: PermitScope::default(),
        max_tokens: None,
        max_wall_seconds: None,
        max_usd: None,
        expires_at: now() + chrono::Duration::hours(1),
        issued_at: now(),
        signature: None,
    }
}

#[test]
fn sign_verify_roundtrip() {
    let sk = SigningKey::generate(&mut OsRng);
    let pk = sk.verifying_key();
    let mut p = sample_permit();
    sign(&mut p, &sk).expect("sign");
    assert!(p.signature.is_some());
    verify(&p, &pk).expect("verify");
}

#[test]
fn tampered_permit_fails_verify() {
    let sk = SigningKey::generate(&mut OsRng);
    let pk = sk.verifying_key();
    let mut p = sample_permit();
    sign(&mut p, &sk).expect("sign");
    // Tamper with the allowlist after signing.
    p.tool_allowlist.0.push("write".into());
    assert!(verify(&p, &pk).is_err());
}

#[test]
fn tool_allowed_checks() {
    let p = sample_permit();
    assert!(tool_allowed(&p, "read"));
    assert!(!tool_allowed(&p, "write"));
}

#[cfg(unix)]
#[test]
fn load_signing_key_rejects_insecure_mode() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("signing.key");
    std::fs::write(&path, "0".repeat(64)).expect("write key");
    let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&path, perms).expect("set_permissions");

    let err = load_signing_key(&path).expect_err("insecure mode should fail load");
    let msg = err.to_string();
    assert!(
        matches!(&err, Error::Config(_)),
        "expected Error::Config(_), got: {err:?}"
    );
    assert!(msg.contains("mode"), "'mode' not in error: {msg}");
    assert!(msg.contains("chmod 600"), "'chmod 600' not in error: {msg}");
}
