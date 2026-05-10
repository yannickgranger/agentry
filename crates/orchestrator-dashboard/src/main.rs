//! orchestrator-dashboard — live view over Redis state.
//!
//! Composition root: wires AppState (Redis-backed store + webhook secret),
//! mounts the Axum router (HTML views + SSE streams from `routes::views`,
//! brief-forensics tree from `routes::briefs`, per-brief metrics from
//! `metrics`), and serves on `cfg.dashboard.port`. Handler bodies live in
//! `routes::views`; this file owns only the cross-cutting endpoints
//! (`webhook_submit`, `healthz`).
//!
//! Routes:
//!   GET /                           index (recent verdicts + active briefs)
//!   GET /brief/:id                  brief detail with live trace
//!   GET /sse/verdicts               SSE stream of new verdicts
//!   GET /sse/brief/:id/trace        SSE stream of brief's trace events
//!   GET /healthz                    liveness
//!   POST /submit                    webhook brief submission (M8)
//!   GET /roles                      list roles
//!   GET /roles/new                  create-role form
//!   POST /roles                     submit role (serde-validated, auto-version)
//!   GET /teams                      list teams
//!   GET /teams/new                  create-team form
//!   POST /teams                     submit team
//!   GET /projects                   list projects
//!   GET /projects/new               create-project form
//!   POST /projects                  submit project

#![forbid(unsafe_code)]

use orchestrator_dashboard::routes::views;
use orchestrator_dashboard::store::DashboardStore;
use orchestrator_dashboard::{metrics, resolve_webhook_secret, routes, AppState};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use orchestrator_infra::Config;
use orchestrator_types::Brief;
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchestrator_dashboard=info,info".into()),
        )
        .init();

    let cfg = Config::load().map_err(|e| anyhow::anyhow!("config load: {e}"))?;
    let port: u16 = cfg.dashboard.port;

    let store = DashboardStore::new(&cfg.redis.url).await?;
    let state = AppState {
        store,
        webhook_secret: resolve_webhook_secret(cfg.webhook.secret.clone())?,
    };

    let app = Router::new()
        .route("/", get(views::index))
        .route("/brief/{id}", get(views::brief_detail))
        .route(
            "/brief/{brief_id}/metrics",
            get(metrics::brief_metrics_handler),
        )
        .route("/sse/verdicts", get(views::sse_verdicts))
        .route("/sse/brief/{id}/trace", get(views::sse_brief_trace))
        .route("/healthz", get(healthz))
        // M8 webhook trigger
        .route("/submit", post(webhook_submit))
        // M2 registry editor
        .route("/roles", get(views::roles_list))
        .route("/roles/new", get(views::role_new_form))
        .route("/roles", post(views::role_create))
        .route("/teams", get(views::teams_list))
        .route("/teams/new", get(views::team_new_form))
        .route("/teams", post(views::team_create))
        .route("/projects", get(views::projects_list))
        .route("/projects/new", get(views::project_new_form))
        .route("/projects", post(views::project_create))
        .with_state(state)
        // Phase 1 substrate-forensics routes (issue #94). Mounted via .merge
        // so existing route URLs are unchanged. The briefs subtree carries
        // its own state (transcripts dir).
        .merge(routes::briefs::router(
            routes::briefs::BriefsState::default(),
        ));

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

// ---------- webhook trigger (M8) ----------

/// POST /submit — accept a Brief JSON, forward to `agentry:briefs`.
/// Guarded by a shared-secret in `X-Agentry-Token`. Returns 401 if the
/// dashboard has no webhook secret configured (disabled), or if the token
/// doesn't match.
async fn webhook_submit(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(brief): Json<Brief>,
) -> Response {
    let Some(expected) = app.webhook_secret.as_deref() else {
        return (
            StatusCode::UNAUTHORIZED,
            "webhook disabled (config.webhook.secret unset)",
        )
            .into_response();
    };
    let provided = headers
        .get("x-agentry-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided != expected {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }

    match app.store.submit_brief(&brief).await {
        Ok(stream_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "submitted": true,
                "brief_id": brief.id.to_string(),
                "stream_id": stream_id,
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "webhook submit failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("submit failed: {e}"),
            )
                .into_response()
        }
    }
}
