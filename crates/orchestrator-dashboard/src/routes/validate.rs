//! Tiny request validators shared by every brief route.
//!
//! Path-traversal defence is two-stage: (1) a charset gate on each
//! user-controlled identifier (`brief_id`, `role` query param) — only
//! `[A-Za-z0-9_-]` is accepted, with a length cap; (2) after constructing a
//! candidate filesystem path, callers MUST run `within_root` to canonicalize
//! and verify the canonical result is still a descendant of the transcript
//! root. The charset gate alone is necessary but not sufficient: a symlink
//! planted in the transcripts dir could still escape, which only
//! canonicalize-then-verify catches.

use axum::http::StatusCode;
use std::path::{Path, PathBuf};

const BRIEF_ID_MAX: usize = 256;
const ROLE_MAX: usize = 128;

/// Charset-validate a brief id against `^[A-Za-z0-9_-]+$` AND a length cap.
pub fn brief_id(id: &str) -> Result<(), (StatusCode, &'static str)> {
    if id.is_empty() || id.len() > BRIEF_ID_MAX {
        return Err((StatusCode::BAD_REQUEST, "invalid brief id"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err((StatusCode::BAD_REQUEST, "invalid brief id"));
    }
    Ok(())
}

/// Charset-validate a role name against `^[A-Za-z0-9_-]+$` AND a length cap.
pub fn role(role: &str) -> Result<(), (StatusCode, &'static str)> {
    if role.is_empty() || role.len() > ROLE_MAX {
        return Err((StatusCode::BAD_REQUEST, "invalid role"));
    }
    if !role
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err((StatusCode::BAD_REQUEST, "invalid role"));
    }
    Ok(())
}

/// Canonicalize `candidate` and verify it is still a descendant of `root`.
/// Both paths are canonicalized so symlinks are resolved before the prefix
/// check — without this, a symlink in the transcripts dir pointing at
/// `/etc/passwd` would slip past the charset gate.
///
/// Returns `Err(404)` when either path doesn't exist (transcripts route
/// surfaces this as 404), and `Err(400)` when canonical `candidate` escapes
/// `root`.
pub async fn within_root(
    candidate: &Path,
    root: &Path,
) -> Result<PathBuf, (StatusCode, &'static str)> {
    let root_canon = match tokio::fs::canonicalize(root).await {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::NOT_FOUND, "transcript not found")),
    };
    let cand_canon = match tokio::fs::canonicalize(candidate).await {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::NOT_FOUND, "transcript not found")),
    };
    if !cand_canon.starts_with(&root_canon) {
        return Err((StatusCode::BAD_REQUEST, "path escapes transcript root"));
    }
    Ok(cand_canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_id_accepts_valid() {
        assert!(brief_id("brf_01HZ").is_ok());
        assert!(brief_id("a").is_ok());
        assert!(brief_id("AbC_123-xyz").is_ok());
    }

    #[test]
    fn brief_id_rejects_traversal_chars() {
        assert!(brief_id("..").is_err());
        assert!(brief_id("../etc/passwd").is_err());
        assert!(brief_id("/etc/shadow").is_err());
        assert!(brief_id("a/b").is_err());
        assert!(brief_id("a.b").is_err());
        assert!(brief_id("").is_err());
    }

    #[test]
    fn brief_id_rejects_overlong() {
        let s: String = "a".repeat(BRIEF_ID_MAX + 1);
        assert!(brief_id(&s).is_err());
    }

    #[test]
    fn role_rejects_traversal() {
        assert!(role("..").is_err());
        assert!(role("../evil").is_err());
        assert!(role("coder").is_ok());
        assert!(role("reviewer-claude").is_ok());
    }

    #[tokio::test]
    async fn within_root_accepts_descendant() {
        let tmp = tempfile::tempdir().expect("tmp");
        let child = tmp.path().join("child.jsonl");
        tokio::fs::write(&child, b"x").await.expect("write");
        let canon = within_root(&child, tmp.path()).await.expect("ok");
        let root_canon = tokio::fs::canonicalize(tmp.path())
            .await
            .expect("canon root");
        assert!(canon.starts_with(&root_canon));
    }

    #[tokio::test]
    async fn within_root_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().expect("tmp");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        let link = tmp.path().join("escape.jsonl");
        std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
        let res = within_root(&link, tmp.path()).await;
        assert!(matches!(res, Err((StatusCode::BAD_REQUEST, _))));
    }
}
