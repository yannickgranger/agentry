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

/// Runtime-config-derived values that the JSON role catalog can reference via
/// template tokens. Built once per seed pass from `Config` and passed into
/// `load_roles_from_dir` so role JSON files can stay declarative without
/// baking forge host / sccache endpoint / allowed-owners list into source.
///
/// Token semantics — see `expand_string_templates` and `expand_role_templates`:
/// * `~/<rest>` and `${HOME}` substring → `home`
/// * `${FORGE_NET_ALLOW}` substring → `forge_net_allow`
/// * `${SCCACHE_NET_ALLOW}` substring → `sccache_net_allow.unwrap_or("")`
/// * `${FORGE_WRITE_PERMITS}` — list-spread token, only valid as a SOLE
///   element in a `permit_scope` entry; expanded by `expand_role_templates`.
pub struct TemplateContext {
    pub home: String,
    pub forge_net_allow: String,
    pub forge_write_permits: Vec<String>,
    pub sccache_net_allow: Option<String>,
}

/// Expand `~/`, `${HOME}`, `${FORGE_NET_ALLOW}`, and `${SCCACHE_NET_ALLOW}`
/// in a single string against the supplied `TemplateContext`. A bare `~`
/// (no slash) is left unmodified to avoid surprise expansion of unrelated
/// tilde-prefixed paths. Only the bracketed `${VAR}` form is expanded —
/// full shell variable expansion is out of scope.
///
/// `${SCCACHE_NET_ALLOW}` substitutes to the empty string when
/// `ctx.sccache_net_allow` is `None`. Callers placing it as a sole permit
/// element should use `expand_role_templates` instead, which drops the
/// element entirely in that case.
///
/// `${FORGE_WRITE_PERMITS}` is intentionally NOT substituted here — it is
/// a list-spread token handled in `expand_role_templates`'s per-element
/// walk over `permit_scope`.
pub fn expand_string_templates(s: &str, ctx: &TemplateContext) -> String {
    let mut out = if let Some(rest) = s.strip_prefix("~/") {
        format!("{}/{}", ctx.home, rest)
    } else {
        s.to_string()
    };
    if out.contains("${HOME}") {
        out = out.replace("${HOME}", &ctx.home);
    }
    if out.contains("${FORGE_NET_ALLOW}") {
        out = out.replace("${FORGE_NET_ALLOW}", &ctx.forge_net_allow);
    }
    if out.contains("${SCCACHE_NET_ALLOW}") {
        let sccache = ctx.sccache_net_allow.as_deref().unwrap_or("");
        out = out.replace("${SCCACHE_NET_ALLOW}", sccache);
    }
    out
}

/// Apply template expansion to every string field of `role` that participates
/// in the templating contract:
///
/// * Each `mount.source` is run through `expand_string_templates`.
/// * Each `permit_scope` entry is processed with list-spread semantics:
///   1. An element exactly equal to `${FORGE_WRITE_PERMITS}` is dropped and
///      replaced inline by `ctx.forge_write_permits` (preserving order).
///   2. An element exactly equal to `${SCCACHE_NET_ALLOW}` with
///      `ctx.sccache_net_allow == None` is dropped entirely (filtered out
///      rather than left as the empty string).
///   3. Otherwise the element is run through `expand_string_templates`.
///
/// Emits `tracing::info!` for every changed value (mount source or permit
/// element) so seed-time substitutions are auditable.
pub fn expand_role_templates(role: &mut AgentRole, ctx: &TemplateContext) {
    for mount in role.mounts.iter_mut() {
        let expanded = expand_string_templates(&mount.source, ctx);
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

    let original = std::mem::take(&mut role.permit_scope.0);
    let mut expanded: Vec<String> = Vec::with_capacity(original.len());
    for entry in original {
        if entry == "${FORGE_WRITE_PERMITS}" {
            for permit in &ctx.forge_write_permits {
                tracing::info!(
                    role_name = %role.name.0,
                    original = %entry,
                    expanded = %permit,
                    "expanded permit_scope element (forge-write spread)",
                );
                expanded.push(permit.clone());
            }
        } else if entry == "${SCCACHE_NET_ALLOW}" && ctx.sccache_net_allow.is_none() {
            tracing::info!(
                role_name = %role.name.0,
                original = %entry,
                "dropped permit_scope element (sccache disabled)",
            );
        } else {
            let new_entry = expand_string_templates(&entry, ctx);
            if new_entry != entry {
                tracing::info!(
                    role_name = %role.name.0,
                    original = %entry,
                    expanded = %new_entry,
                    "expanded permit_scope element",
                );
            }
            expanded.push(new_entry);
        }
    }
    role.permit_scope.0 = expanded;
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
///
/// Every loaded role is run through `expand_role_templates` against `ctx`,
/// so JSON files may reference runtime-config-derived values via the
/// template tokens documented on `TemplateContext`.
pub async fn load_roles_from_dir(
    conn: &mut ConnectionManager,
    dir: &Path,
    ctx: &TemplateContext,
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

    let mut out: Vec<RoleName> = Vec::with_capacity(json_files.len());
    for path in json_files {
        let mut role = read_and_parse_role(&path).await?;
        expand_role_templates(&mut role, ctx);
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
