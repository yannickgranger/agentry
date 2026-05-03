//! Seed-from-directory loader for role JSON catalogs.
//!
//! Reads every `*.json` file in a given directory, deserializes it as
//! `AgentRole` under strict serde (unknown fields rejected), and persists
//! each via `redis_io::save_role`. The directory is optional — if it does
//! not exist the loader returns an empty `Vec` so substrates that don't ship
//! a role catalog can still call seed unconditionally.

use crate::{redis_io, Error, Result};
use orchestrator_types::{AgentRole, RoleName};
use redis::aio::ConnectionManager;
use std::path::Path;

/// Expand `~/` and `${HOME}` substrings in a Mount.source path against the
/// substrate-side `$HOME`. A bare `~` (no slash) is left unmodified to avoid
/// surprise expansion of unrelated tilde-prefixed paths. Only the bracketed
/// `${HOME}` form is expanded — full shell variable expansion is out of scope.
fn expand_home_in_source(source: &str, home: &str) -> String {
    let mut out = if let Some(rest) = source.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        source.to_string()
    };
    if out.contains("${HOME}") {
        out = out.replace("${HOME}", home);
    }
    out
}

/// Read and deserialize a single role JSON file. Both file-read and parse
/// errors surface as `Error::RoleLoadFailed` so the offending path is named.
///
/// Exposed so integration tests (and any caller that wants the parse path
/// without persistence) can exercise the structured-error contract without
/// a live Redis connection.
pub async fn read_and_parse_role(path: &Path) -> Result<AgentRole> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| Error::RoleLoadFailed {
            path: path.to_path_buf(),
            source: Box::new(e),
        })?;
    serde_json::from_str::<AgentRole>(&text).map_err(|e| Error::RoleLoadFailed {
        path: path.to_path_buf(),
        source: Box::new(e),
    })
}

/// Load every `*.json` role file in `dir` into Redis. Returns the list of
/// names registered, in the order the files were processed (sorted by file
/// name for determinism).
///
/// * If `dir` does not exist, returns `Ok(vec![])` — silent skip.
/// * If a file fails to deserialize, the error is propagated wrapped with
///   the file path in the context.
pub async fn load_roles_from_dir(
    conn: &mut ConnectionManager,
    dir: &Path,
) -> Result<Vec<RoleName>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut json_files: Vec<std::path::PathBuf> = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            json_files.push(path);
        }
    }
    json_files.sort();

    let home = std::env::var("HOME").expect("HOME env var must be set to derive role-dir paths");

    let mut out: Vec<RoleName> = Vec::with_capacity(json_files.len());
    for path in json_files {
        let mut role = read_and_parse_role(&path).await?;
        for mount in role.mounts.iter_mut() {
            let expanded = expand_home_in_source(&mount.source, &home);
            if expanded != mount.source {
                tracing::info!(
                    role_name = %role.name.0,
                    original = %mount.source,
                    expanded = %expanded,
                    "expanded mount source",
                );
                mount.source = expanded;
            }
        }
        redis_io::save_role(conn, &role)
            .await
            .map_err(|e| Error::RoleLoadFailed {
                path: path.clone(),
                source: Box::new(e),
            })?;
        tracing::info!(
            role_name = %role.name.0,
            file_path = %path.display(),
            "loaded role from JSON file",
        );
        out.push(role.name);
    }
    Ok(out)
}
