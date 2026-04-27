//! HTTP routes mounted under `/briefs/{id}` — operator-facing read access
//! to per-brief transcripts, the brief's host workspace path, and a
//! kill-switch for the brief's running role container.
//!
//! Phase 1 of substrate forensics. All routes are read-only except
//! `POST /briefs/{id}/kill`. No auth on the dashboard today; traversal
//! defence is enforced per-route via `super::validate`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use orchestrator_runtime::transcript;
use orchestrator_types::BriefId;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

use super::validate;

/// Default location where `BASH_PRELUDE::stream_claude` writes per-call
/// transcripts.
const DEFAULT_TRANSCRIPT_DIR: &str = "/var/lib/agentry/transcripts";

/// Hard cap on `?n=` for `/transcript/tail`.
const TAIL_MAX: usize = 1000;
const TAIL_DEFAULT: usize = 20;

#[derive(Clone)]
pub struct BriefsState {
    transcript_dir: Arc<PathBuf>,
}

impl BriefsState {
    #[must_use]
    pub fn new(transcript_dir: PathBuf) -> Self {
        Self {
            transcript_dir: Arc::new(transcript_dir),
        }
    }

    fn transcript_dir(&self) -> &PathBuf {
        &self.transcript_dir
    }
}

impl Default for BriefsState {
    fn default() -> Self {
        Self::new(PathBuf::from(DEFAULT_TRANSCRIPT_DIR))
    }
}

/// Build the brief routes subtree. Mount via `.merge` so it adds new paths
/// without rewriting existing ones.
pub fn router(state: BriefsState) -> Router {
    Router::new()
        .route("/briefs/{id}/transcript", get(get_transcript))
        .route("/briefs/{id}/transcript/tail", get(get_transcript_tail))
        .route(
            "/briefs/{id}/transcript/last-tool-call",
            get(get_last_tool_call),
        )
        .route("/briefs/{id}/transcript/summary", get(get_summary))
        .route("/briefs/{id}/workspace/path", get(get_workspace_path))
        .route("/briefs/{id}/kill", post(post_kill))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct RoleQuery {
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TailQuery {
    role: Option<String>,
    n: Option<usize>,
}

/// Resolve the on-disk transcript path for `(brief_id, role)`. The role
/// suffix follows `BASH_PRELUDE::stream_claude`'s convention: `""` for the
/// default coder transcript, `.<role>` otherwise. Returns the canonical
/// path on success.
async fn resolve_transcript(
    state: &BriefsState,
    brief_id: &str,
    role: Option<&str>,
) -> Result<PathBuf, (StatusCode, &'static str)> {
    validate::brief_id(brief_id)?;
    let suffix = match role {
        Some(r) => {
            validate::role(r)?;
            format!(".{r}")
        }
        None => String::new(),
    };
    let candidate = state
        .transcript_dir()
        .join(format!("{brief_id}{suffix}.jsonl"));
    validate::within_root(&candidate, state.transcript_dir()).await
}

async fn get_transcript(
    State(state): State<BriefsState>,
    Path(id): Path<String>,
    Query(q): Query<RoleQuery>,
) -> Response {
    let path = match resolve_transcript(&state, &id, q.role.as_deref()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "transcript not found").into_response(),
    }
}

async fn get_transcript_tail(
    State(state): State<BriefsState>,
    Path(id): Path<String>,
    Query(q): Query<TailQuery>,
) -> Response {
    let n = q.n.unwrap_or(TAIL_DEFAULT).min(TAIL_MAX);
    let path = match resolve_transcript(&state, &id, q.role.as_deref()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let body = match tokio::fs::read_to_string(&path).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "transcript not found").into_response(),
    };
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.len().saturating_sub(n);
    let tail = lines[start..].join("\n");
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        tail,
    )
        .into_response()
}

async fn get_last_tool_call(
    State(state): State<BriefsState>,
    Path(id): Path<String>,
    Query(q): Query<RoleQuery>,
) -> Response {
    let path = match resolve_transcript(&state, &id, q.role.as_deref()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let body = match tokio::fs::read_to_string(&path).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "transcript not found").into_response(),
    };
    let events = transcript::parse_jsonl_lines(&body);
    match transcript::extract_last_tool_call(&events) {
        Some(call) => (StatusCode::OK, Json(call)).into_response(),
        None => (StatusCode::NOT_FOUND, "no tool calls in transcript").into_response(),
    }
}

async fn get_summary(
    State(state): State<BriefsState>,
    Path(id): Path<String>,
    Query(q): Query<RoleQuery>,
) -> Response {
    let path = match resolve_transcript(&state, &id, q.role.as_deref()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let body = match tokio::fs::read_to_string(&path).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "transcript not found").into_response(),
    };
    let events = transcript::parse_jsonl_lines(&body);
    let summary = transcript::summarize(&events);
    (StatusCode::OK, Json(summary)).into_response()
}

async fn get_workspace_path(State(_state): State<BriefsState>, Path(id): Path<String>) -> Response {
    if let Err(e) = validate::brief_id(&id) {
        return e.into_response();
    }
    let bid = BriefId(id);
    match orchestrator_runtime::spawner::workspace_path(&bid) {
        Some(p) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            p.display().to_string(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no workspace for brief").into_response(),
    }
}

async fn post_kill(State(_state): State<BriefsState>, Path(id): Path<String>) -> Response {
    if let Err(e) = validate::brief_id(&id) {
        return e.into_response();
    }
    let bid = BriefId(id);
    match orchestrator_runtime::spawner::kill(&bid).await {
        Ok(()) => (StatusCode::ACCEPTED, "SIGTERM signaled").into_response(),
        Err(orchestrator_runtime::Error::NotFound { .. }) => {
            (StatusCode::NOT_FOUND, "no running container for brief").into_response()
        }
        Err(e) => {
            tracing::warn!(brief = %bid, error = %e, "kill failed");
            (StatusCode::SERVICE_UNAVAILABLE, "kill signal failed").into_response()
        }
    }
}
