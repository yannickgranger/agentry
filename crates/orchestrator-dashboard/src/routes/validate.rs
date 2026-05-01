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
