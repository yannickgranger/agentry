//! Permit broker — ed25519 signing, verification, tool-call enforcement (M3).
//!
//! Not a salvage of agency-aegis's code — just its shape:
//!   * ed25519 keypair stored at `$AGENTRY_SIGNING_KEY` or `~/.config/agentry/signing.key`
//!   * Permit is signed by canonicalizing the WorkPermit (without `signature`) as JSON
//!   * Verify at tool-call time: signature must match; tool must be in allowlist
//!
//! Agents never see the signing key; they receive a fully-signed permit in
//! their startup bundle. If they try to call a tool outside the allowlist,
//! the broker kills the container and records a `permit_violation` verdict.

use crate::{Config, Error, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use orchestrator_types::WorkPermit;
use rand_core::OsRng;
use std::path::Path;

/// Resolve the signing-key path from the loaded Config.
/// Callers that don't already have a Config should call `Config::load()?` first.
#[must_use]
pub fn key_path_from(cfg: &Config) -> &Path {
    &cfg.signing.key_path
}

/// Generate a fresh ed25519 keypair and write it to `path`. 0600 perms.
pub fn generate_and_save(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let sk = SigningKey::generate(&mut OsRng);
    let bytes = sk.to_bytes();
    std::fs::write(path, hex::encode(bytes))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Load a signing key from disk (hex-encoded 32 bytes).
pub fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let hex_str = std::fs::read_to_string(path)?;
    let bytes = hex::decode(hex_str.trim())
        .map_err(|e| Error::Config(format!("signing key hex decode: {e}")))?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Config("signing key must be 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&arr))
}

/// Canonical bytes over which a permit's signature is computed: the JSON
/// encoding of the permit with `signature = None`.
fn canonical_bytes(permit: &WorkPermit) -> Result<Vec<u8>> {
    let mut stripped = permit.clone();
    stripped.signature = None;
    Ok(serde_json::to_vec(&stripped)?)
}

/// Sign a permit in place. After this, `permit.signature` is `Some(hex)`.
pub fn sign(permit: &mut WorkPermit, key: &SigningKey) -> Result<()> {
    let bytes = canonical_bytes(permit)?;
    let sig: Signature = key.sign(&bytes);
    permit.signature = Some(hex::encode(sig.to_bytes()));
    Ok(())
}

/// Verify a permit's signature. Returns `Err` if signature is missing or invalid.
pub fn verify(permit: &WorkPermit, pub_key: &VerifyingKey) -> Result<()> {
    let sig_hex = permit
        .signature
        .as_ref()
        .ok_or_else(|| Error::Config("permit has no signature".into()))?;
    let sig_bytes = hex::decode(sig_hex)
        .map_err(|e| Error::Config(format!("signature hex decode: {e}")))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Config("signature must be 64 bytes".into()))?;
    let sig = Signature::from_bytes(&sig_arr);
    let bytes = canonical_bytes(permit)?;
    pub_key
        .verify(&bytes, &sig)
        .map_err(|e| Error::Config(format!("signature invalid: {e}")))?;
    Ok(())
}

/// Check if a tool call is allowed by the permit.
#[must_use]
pub fn tool_allowed(permit: &WorkPermit, tool: &str) -> bool {
    permit.allows(tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_types::{
        BriefId, PermitScope, RoleName, ToolAllowlist, WorkPermit, now,
    };

    fn sample_permit() -> WorkPermit {
        WorkPermit {
            permit_id: "prm_t".into(),
            agent_id: "agt_t".into(),
            role: RoleName("t".into()),
            brief: BriefId("brf_t".into()),
            tool_allowlist: ToolAllowlist(vec!["read".into()]),
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
}
