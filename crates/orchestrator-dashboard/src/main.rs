//! orchestrator-dashboard — live view over Redis state.
//!
//! Single-file dashboard (M1+M2): Axum + SSE + hand-rolled HTML + Tailwind CDN.
//! M2 adds typed registry editor: list + create for AgentRole, TeamTopology, Project.
//!
//! Routes:
//!   GET /                           index (recent verdicts + active briefs)
//!   GET /brief/:id                  brief detail with live trace
//!   GET /sse/verdicts               SSE stream of new verdicts
//!   GET /sse/brief/:id/trace        SSE stream of brief's trace events
//!   GET /healthz                    liveness
//!   GET /roles                      list roles
//!   GET /roles/new                  create-role form
//!   POST /roles                     submit role (serde-validated, auto-version)
//!   GET /teams                      list teams
//!   GET /teams/new                  create-team form
//!   POST /teams                     submit team
//!   GET /projects                   list projects
//!   GET /projects/new               create-project form
//!   POST /projects                  submit project
//!
//! The SSE handlers tail Redis streams; the HTML bootstraps the latest entries
//! and the EventSource live-appends as new entries arrive.

#![forbid(unsafe_code)]

use orchestrator_dashboard::store::DashboardStore;
use orchestrator_dashboard::{metrics, resolve_webhook_secret, routes};

use axum::extract::{Form, FromRef, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream, StreamExt};
use orchestrator_infra::redis_io;
use orchestrator_infra::Config;
use orchestrator_types::lifecycle::{BriefState, BriefStateRecord};
use orchestrator_types::{
    brief::EscalationMode, role::McpServer, AgentRole, Brief, MessageEdge, PackageManager,
    PermitScope, Project, ProjectSlug, RoleName, RoleRef, StandingOrders, SubstrateClass, TeamName,
    TeamTopology, ToolAllowlist,
};
use redis::AsyncCommands;
use serde::Deserialize;
use serde_json::Value;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Clone)]
struct AppState {
    store: DashboardStore,
    webhook_secret: Option<String>,
}

impl FromRef<AppState> for DashboardStore {
    fn from_ref(state: &AppState) -> Self {
        state.store.clone()
    }
}

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
        .route("/", get(index))
        .route("/brief/{id}", get(brief_detail))
        .route(
            "/brief/{brief_id}/metrics",
            get(metrics::brief_metrics_handler),
        )
        .route("/sse/verdicts", get(sse_verdicts))
        .route("/sse/brief/{id}/trace", get(sse_brief_trace))
        .route("/healthz", get(healthz))
        // M8 webhook trigger
        .route("/submit", post(webhook_submit))
        // M2 registry editor
        .route("/roles", get(roles_list))
        .route("/roles/new", get(role_new_form))
        .route("/roles", post(role_create))
        .route("/teams", get(teams_list))
        .route("/teams/new", get(team_new_form))
        .route("/teams", post(team_create))
        .route("/projects", get(projects_list))
        .route("/projects/new", get(project_new_form))
        .route("/projects", post(project_create))
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

/// Derive the set of briefs in flight from the FSM state-key projection
/// (`agentry:brief:*:state`) and materialise their bodies. Returns the
/// same shape `active_briefs` previously produced (a Vec of brief-body
/// JSON values) so the index template's field reads keep working.
async fn derive_active_briefs(store: &DashboardStore) -> anyhow::Result<Vec<Value>> {
    let mut conn = redis_io::connect(store.redis_url())
        .await
        .map_err(|e| anyhow::anyhow!("redis connect: {e}"))?;

    let mut state_keys: Vec<String> = Vec::new();
    {
        let mut iter = conn
            .scan_match::<_, String>("agentry:brief:*:state")
            .await?;
        while let Some(k) = iter.next_item().await {
            state_keys.push(k);
        }
    }

    let mut active_ids: Vec<String> = Vec::new();
    for key in &state_keys {
        let raw: Option<String> = conn.get(key).await?;
        let Some(s) = raw else { continue };
        let Ok(record) = serde_json::from_str::<BriefStateRecord>(&s) else {
            continue;
        };
        if !matches!(
            record.state,
            BriefState::Shipped | BriefState::Failed { .. }
        ) {
            active_ids.push(record.brief_id.0);
        }
    }

    if active_ids.is_empty() {
        return Ok(Vec::new());
    }

    let body_keys: Vec<String> = active_ids
        .iter()
        .map(|id| format!("agentry:brief:{id}:body"))
        .collect();
    let bodies: Vec<Option<String>> = conn.mget(&body_keys).await?;
    let mut out = Vec::with_capacity(bodies.len());
    for body in bodies.into_iter().flatten() {
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            out.push(v);
        }
    }
    Ok(out)
}

// ---------- error wrapper so `?` works in handlers ----------

struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "handler error");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("error: {}", self.0),
        )
            .into_response()
    }
}

// ---------- index ----------

async fn index(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let active = derive_active_briefs(&state.store).await?;
    let verdicts = state.store.fetch_recent_verdicts(20).await?;

    // Best-effort per-brief metric badges. Aggregation is sync (blocks
    // on a fresh redis connection inside spawn_blocking); fan them out
    // with `join_all` so a slow brief doesn't serialise the page render.
    // Errors collapse to an empty badge so the listing still renders.
    let redis_url = state.store.redis_url().to_string();
    let badge_futs = active.iter().map(|b| {
        let brief_id = b
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let url = redis_url.clone();
        async move {
            if brief_id.is_empty() {
                String::new()
            } else {
                metrics::try_badge(&url, &brief_id).await
            }
        }
    });
    let badges: Vec<String> = futures::future::join_all(badge_futs).await;

    let mut active_items = String::new();
    for (b, badge) in active.iter().zip(badges.iter()) {
        let brief_id = b.get("id").and_then(Value::as_str).unwrap_or("?");
        let topology = b
            .get("topology")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let at = b.get("submitted_at").and_then(Value::as_str).unwrap_or("");
        active_items.push_str(&format!(
            r#"<li class="py-1 border-b border-slate-800 last:border-0">
  <a class="text-indigo-300 hover:text-indigo-200 font-mono text-sm" href="/brief/{brief_id}">{brief_id}</a>
  <span class="text-slate-400 text-xs mx-2">{topology}</span>
  <span class="text-slate-500 text-xs">{at}</span>
  {badge}
</li>"#
        ));
    }
    if active_items.is_empty() {
        active_items
            .push_str(r#"<li class="text-slate-500 italic text-sm">No briefs in flight.</li>"#);
    }

    let initial_verdicts = serde_json::to_string(&verdicts).unwrap_or_else(|_| "[]".into());

    Ok(Html(page(
        "agentry — dashboard",
        &format!(
            r#"
<section class="mb-8">
  <h2 class="text-lg font-semibold text-slate-200 mb-2">Briefs in flight</h2>
  <ul id="active-briefs">{active_items}</ul>
</section>

<section>
  <h2 class="text-lg font-semibold text-slate-200 mb-2">Recent verdicts</h2>
  <ul id="verdicts"></ul>
</section>

<script>
function renderVerdict(v) {{
    const brief = v.brief || "?";
    const kind = v.kind || "?";
    const at = v.at || "";
    const badge = kind === "shipped"
        ? "bg-emerald-900 text-emerald-200"
        : (["failed", "permit_violation", "budget_exceeded", "aborted"].includes(kind)
            ? "bg-rose-900 text-rose-200"
            : (kind === "escalated"
                ? "bg-amber-900 text-amber-200"
                : "bg-slate-800 text-slate-200"));
    // Brief 239 fence: surface a Blocker badge when a Shipped verdict
    // carries a non-zero refusal_count — the coder may have produced a
    // passing diff while routed-around denied tools.
    const refusals = (typeof v.refusal_count === "number") ? v.refusal_count : 0;
    const anomaly = (kind === "shipped" && refusals > 0)
        ? `<span class="anomaly-badge ml-2 px-2 py-0.5 rounded text-xs font-semibold bg-rose-700 text-rose-50" title="This brief shipped with ${{refusals}} tool-permission refusals — the coder may have produced a passing diff while routed-around denied tools. Review needed.">⚠ refusal-on-shipped anomaly</span>`
        : "";
    return `<li class="py-1 border-b border-slate-800 last:border-0">
<a class="text-indigo-300 hover:text-indigo-200 font-mono text-sm" href="/brief/${{brief}}">${{brief}}</a>
<span class="mx-2 px-2 py-0.5 rounded text-xs ${{badge}}">${{kind}}</span>
<span class="text-slate-500 text-xs">${{at}}</span>${{anomaly}}</li>`;
}}

const verdictsList = document.getElementById("verdicts");
const initialVerdicts = {initial_verdicts};
if (initialVerdicts.length === 0) {{
    verdictsList.innerHTML = `<li class="text-slate-500 italic text-sm">No verdicts yet.</li>`;
}} else {{
    for (const v of initialVerdicts) {{
        verdictsList.insertAdjacentHTML("beforeend", renderVerdict(v));
    }}
}}

const es = new EventSource("/sse/verdicts");
es.addEventListener("verdict", (e) => {{
    const v = JSON.parse(e.data);
    if (verdictsList.querySelector("li.italic")) verdictsList.innerHTML = "";
    verdictsList.insertAdjacentHTML("afterbegin", renderVerdict(v));
}});
es.onerror = () => {{ /* auto-reconnect */ }};
</script>
"#
        ),
    )))
}

// ---------- brief detail ----------

async fn brief_detail(Path(id): Path<String>) -> Html<String> {
    let body = format!(
        r#"
<p class="mb-4"><a class="text-indigo-300 hover:text-indigo-200" href="/">&larr; back</a></p>

<h2 class="text-lg font-semibold text-slate-200 mb-2">Brief <span class="font-mono">{id}</span></h2>

<ul id="trace" class="space-y-1 text-sm"></ul>

<script>
function renderEvent(e) {{
    const type_ = e.type || "?";
    const at = e.at || "";
    const pill = {{
        event: "bg-slate-700 text-slate-200",
        tool_call: "bg-indigo-900 text-indigo-200",
        message: "bg-cyan-900 text-cyan-200",
        log: "bg-slate-800 text-slate-400",
        done: (e.verdict === "shipped" ? "bg-emerald-900 text-emerald-200" : "bg-rose-900 text-rose-200"),
    }}[type_] || "bg-slate-800 text-slate-300";
    const detail = (() => {{
        if (type_ === "done") return `verdict=<b>${{e.verdict}}</b>`;
        if (type_ === "tool_call") return `${{e.call.tool}} ${{JSON.stringify(e.call.args || {{}})}}`;
        if (type_ === "message") return `to=${{e.to}} ${{JSON.stringify(e.payload || {{}})}}`;
        if (type_ === "log") return `[${{e.level || "info"}}] ${{e.msg || ""}}`;
        return JSON.stringify(e.payload || {{}});
    }})();
    return `<li class="py-1 border-b border-slate-800 last:border-0">
<span class="text-slate-500 font-mono text-xs">${{at}}</span>
<span class="mx-2 px-2 py-0.5 rounded text-xs ${{pill}}">${{type_}}</span>
<span class="text-slate-300">${{detail}}</span></li>`;
}}

const traceList = document.getElementById("trace");
const es = new EventSource("/sse/brief/{id}/trace?from=0-0");
es.addEventListener("event", (e) => {{
    const data = JSON.parse(e.data);
    if (traceList.querySelector("li.italic")) traceList.innerHTML = "";
    traceList.insertAdjacentHTML("beforeend", renderEvent(data));
    window.scrollTo(0, document.body.scrollHeight);
}});
es.onerror = () => {{ /* auto-reconnect */ }};
</script>
"#
    );

    Html(page(&format!("agentry — {id}"), &body))
}

// ---------- SSE ----------

#[derive(Deserialize, Default)]
struct TraceQuery {
    #[serde(default)]
    from: Option<String>,
}

async fn sse_verdicts(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.store.subscribe_verdicts();
    let live = BroadcastStream::new(rx).filter_map(|res| async move {
        match res {
            Ok(body) => Some(Ok::<_, Infallible>(
                Event::default().event("verdict").data(body),
            )),
            Err(_) => None,
        }
    });
    Sse::new(live).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn sse_brief_trace(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Query(q): Query<TraceQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let history = if q.from.as_deref() == Some("0-0") {
        state.store.fetch_trace(&id, 200).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let history_stream = stream::iter(history.into_iter().filter_map(|ev| {
        serde_json::to_string(&ev)
            .ok()
            .map(|body| Ok::<_, Infallible>(Event::default().event("event").data(body)))
    }));
    let rx = state.store.subscribe_trace(&id);
    let live = BroadcastStream::new(rx).filter_map(|res| async move {
        match res {
            Ok(body) => Some(Ok::<_, Infallible>(
                Event::default().event("event").data(body),
            )),
            Err(_) => None,
        }
    });
    Sse::new(history_stream.chain(live))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// ---------- M2 registry editor ----------

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

fn split_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

// ---------- Roles ----------

async fn roles_list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let items: Vec<(String, AgentRole)> = state.store.list::<AgentRole>("role").await?;
    let mut rows = String::new();
    for (key, r) in &items {
        let name = r.name.0.as_str();
        let version = r.version;
        let model = r.model.as_deref().unwrap_or("");
        let image = r.image.as_str();
        rows.push_str(&format!(
            r#"<tr class="border-b border-slate-800">
<td class="py-2 font-mono text-sm text-slate-200">{name}</td>
<td class="py-2 font-mono text-xs text-slate-400">v{version}</td>
<td class="py-2 text-sm text-slate-300">{model}</td>
<td class="py-2 text-xs text-slate-400 font-mono">{image}</td>
<td class="py-2 text-xs text-slate-600 font-mono">{key}</td>
</tr>"#
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="5" class="py-4 text-slate-500 italic text-sm">No roles yet.</td></tr>"#.into();
    }
    Ok(Html(page(
        "agentry — roles",
        &format!(
            r#"<div class="flex items-center mb-4">
<h2 class="text-lg font-semibold text-slate-200">Roles</h2>
<a href="/roles/new" class="ml-auto px-3 py-1 rounded bg-indigo-700 hover:bg-indigo-600 text-white text-sm">+ new role</a>
</div>
<table class="w-full">
<thead><tr class="text-slate-500 text-xs uppercase tracking-wider border-b border-slate-700">
<th class="text-left py-2">name</th><th class="text-left py-2">version</th>
<th class="text-left py-2">model</th><th class="text-left py-2">image</th><th class="text-left py-2">redis key</th></tr></thead>
<tbody>{rows}</tbody></table>"#
        ),
    )))
}

async fn role_new_form() -> Html<String> {
    let body = r##"<h2 class="text-lg font-semibold text-slate-200 mb-4">New role</h2>
<form method="POST" action="/roles" class="space-y-4">
  <div><label class="block text-sm text-slate-400 mb-1">name <span class="text-xs text-slate-600">(lowercase, hyphens)</span></label>
    <input name="name" required pattern="[a-z0-9-]+" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">model <span class="text-xs text-slate-600">(optional)</span></label>
    <input name="model" placeholder="claude-opus-4-7 / grok-4 / gemini-2-flash" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">base image <span class="text-xs text-slate-600">(stock public image; spawner installs binaries + execs entrypoint)</span></label>
    <input name="image" required placeholder="docker.io/library/alpine:3.21" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">substrate</label>
    <select name="substrate_class" class="bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100">
      <option value="podman" selected>podman</option><option value="docker">docker</option>
      <option value="lxc">lxc</option><option value="ssh">ssh</option><option value="vm">vm</option>
    </select></div>
  <div><label class="block text-sm text-slate-400 mb-1">package manager <span class="text-xs text-slate-600">(alpine:apk | debian:apt)</span></label>
    <select name="package_manager" required class="bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100">
      <option value="apk" selected>apk (alpine)</option><option value="apt">apt (debian/ubuntu)</option>
    </select></div>
  <div><label class="block text-sm text-slate-400 mb-1">entrypoint script <span class="text-xs text-slate-600">(bash; reads startup JSON on stdin, emits NDJSON events on stdout)</span></label>
    <textarea name="entrypoint_script" rows="10" required placeholder="#!/usr/bin/env bash&#10;set -euo pipefail&#10;cat > /dev/null&#10;printf '{&quot;at&quot;:&quot;%s&quot;,&quot;type&quot;:&quot;done&quot;,&quot;verdict&quot;:&quot;shipped&quot;}\n' &quot;$(date -Iseconds)&quot;" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-xs text-slate-100"></textarea></div>
  <div><label class="block text-sm text-slate-400 mb-1">system prompt <span class="text-xs text-slate-600">(optional, or @file://path)</span></label>
    <textarea name="system_prompt" rows="4" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-sm text-slate-100"></textarea></div>
  <div><label class="block text-sm text-slate-400 mb-1">binaries <span class="text-xs text-slate-600">(CSV; extras on top of baseline bash/coreutils/jq/ca-certificates)</span></label>
    <input name="binaries_csv" placeholder="git,curl" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">tool allowlist <span class="text-xs text-slate-600">(CSV)</span></label>
    <input name="tool_allowlist_csv" placeholder="read,edit,bash:cargo" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">permit scope <span class="text-xs text-slate-600">(one per line)</span></label>
    <textarea name="permit_scope_lines" rows="3" placeholder="fs:read:/workspace/**&#10;fs:write:/workspace/**&#10;net:deny:*" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></textarea></div>
  <div><label class="block text-sm text-slate-400 mb-1">mcp servers <span class="text-xs text-slate-600">(JSON array, optional)</span></label>
    <textarea name="mcp_servers_json" rows="3" placeholder='[{"name":"ra-query","image":"ghcr.io/yg/ra-query:v0.1"}]' class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></textarea></div>
  <div><label class="block text-sm text-slate-400 mb-1">passthru env <span class="text-xs text-slate-600">(CSV of env-var names read from orchestratord env, e.g. XAI_API_KEY)</span></label>
    <input name="passthru_env_csv" placeholder="XAI_API_KEY,GEMINI_API_KEY" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">bind mounts <span class="text-xs text-slate-600">(one per line: <code>source:target[:ro]</code>)</span></label>
    <textarea name="mounts_lines" rows="3" placeholder="/var/home/yg/.local/bin/claude:/usr/local/bin/claude:ro&#10;/var/home/yg/.claude/.credentials.json:/root/.claude/.credentials.json:ro" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></textarea></div>
  <button type="submit" class="px-4 py-2 rounded bg-indigo-700 hover:bg-indigo-600 text-white">save (auto v=next)</button>
</form>"##;
    Html(page("agentry — new role", body))
}

#[derive(Deserialize)]
struct RoleForm {
    name: String,
    model: Option<String>,
    image: String,
    substrate_class: String,
    package_manager: String,
    entrypoint_script: String,
    system_prompt: Option<String>,
    binaries_csv: String,
    tool_allowlist_csv: String,
    permit_scope_lines: String,
    mcp_servers_json: String,
    #[serde(default)]
    passthru_env_csv: String,
    #[serde(default)]
    mounts_lines: String,
}

/// Parse `source:target[:ro]` lines into `Mount` records.
fn parse_mounts(s: &str) -> Vec<orchestrator_types::Mount> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split(':').collect();
            match parts.as_slice() {
                [src, tgt] => Some(orchestrator_types::Mount {
                    source: (*src).into(),
                    target: (*tgt).into(),
                    readonly: false,
                }),
                [src, tgt, "ro"] => Some(orchestrator_types::Mount {
                    source: (*src).into(),
                    target: (*tgt).into(),
                    readonly: true,
                }),
                _ => None,
            }
        })
        .collect()
}

async fn role_create(
    State(state): State<AppState>,
    Form(f): Form<RoleForm>,
) -> Result<Redirect, AppError> {
    let substrate_class: SubstrateClass =
        serde_json::from_value(Value::String(f.substrate_class.clone()))
            .map_err(|e| anyhow::anyhow!("invalid substrate_class: {e}"))?;
    let package_manager: PackageManager =
        serde_json::from_value(Value::String(f.package_manager.clone()))
            .map_err(|e| anyhow::anyhow!("invalid package_manager: {e}"))?;

    if f.entrypoint_script.trim().is_empty() {
        return Err(anyhow::anyhow!("entrypoint_script is required").into());
    }

    let mcp_servers: Vec<McpServer> = if f.mcp_servers_json.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&f.mcp_servers_json)
            .map_err(|e| anyhow::anyhow!("mcp_servers_json: {e}"))?
    };

    let model = f.model.filter(|s| !s.trim().is_empty());
    let system_prompt = f.system_prompt.filter(|s| !s.trim().is_empty());

    let version = state.store.next_version("role", &f.name).await?;
    let role = AgentRole {
        name: RoleName(f.name.clone()),
        version,
        model,
        system_prompt,
        image: f.image,
        substrate_class,
        package_manager,
        entrypoint_script: f.entrypoint_script,
        exitpoint_script: None,
        binaries: split_csv(&f.binaries_csv),
        mcp_servers,
        tool_allowlist: ToolAllowlist(split_csv(&f.tool_allowlist_csv)),
        allowed_tools: None,
        permit_scope: PermitScope(split_lines(&f.permit_scope_lines)),
        passthru_env: split_csv(&f.passthru_env_csv),
        extra_bootstrap: vec![],
        mounts: parse_mounts(&f.mounts_lines),
        // Dashboard form doesn't surface workspace_mount yet; dashboard-
        // created roles default to no workspace. Future issue extends the
        // form when a dashboard-author wants a workspace-using role.
        workspace_mount: None,
        sccache: false,
        tool_packs: vec![],
    };
    state
        .store
        .save("role", &role.name.0, role.version, &role)
        .await?;
    Ok(Redirect::to("/roles"))
}

// ---------- Teams ----------

async fn teams_list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let items: Vec<(String, TeamTopology)> = state.store.list::<TeamTopology>("team").await?;
    let mut rows = String::new();
    for (key, t) in &items {
        let name = t.name.0.as_str();
        let version = t.version;
        let roles = t
            .roles
            .iter()
            .map(|r| format!("{}@v{}", r.name.0, r.version))
            .collect::<Vec<_>>()
            .join(", ");
        let terminal_owned = format!("{}@v{}", t.terminal_role.name.0, t.terminal_role.version);
        let terminal = terminal_owned.as_str();
        rows.push_str(&format!(
            r#"<tr class="border-b border-slate-800">
<td class="py-2 font-mono text-sm text-slate-200">{name}</td>
<td class="py-2 font-mono text-xs text-slate-400">v{version}</td>
<td class="py-2 text-sm text-slate-300">{roles}</td>
<td class="py-2 text-xs font-mono text-slate-400">{terminal}</td>
<td class="py-2 text-xs text-slate-600 font-mono">{key}</td>
</tr>"#
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="5" class="py-4 text-slate-500 italic text-sm">No teams yet.</td></tr>"#.into();
    }
    Ok(Html(page(
        "agentry — teams",
        &format!(
            r#"<div class="flex items-center mb-4">
<h2 class="text-lg font-semibold text-slate-200">Teams</h2>
<a href="/teams/new" class="ml-auto px-3 py-1 rounded bg-indigo-700 hover:bg-indigo-600 text-white text-sm">+ new team</a>
</div>
<table class="w-full">
<thead><tr class="text-slate-500 text-xs uppercase tracking-wider border-b border-slate-700">
<th class="text-left py-2">name</th><th class="text-left py-2">version</th>
<th class="text-left py-2">roles</th><th class="text-left py-2">terminal</th><th class="text-left py-2">redis key</th></tr></thead>
<tbody>{rows}</tbody></table>"#
        ),
    )))
}

async fn team_new_form() -> Html<String> {
    let body = r#"<h2 class="text-lg font-semibold text-slate-200 mb-4">New team</h2>
<form method="POST" action="/teams" class="space-y-4">
  <div><label class="block text-sm text-slate-400 mb-1">name</label>
    <input name="name" required pattern="[a-z0-9-]+" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">roles <span class="text-xs text-slate-600">(CSV, in order)</span></label>
    <input name="roles_csv" required placeholder="archaeologist,prescriber,coder-rust,reviewer,shipper" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">message graph <span class="text-xs text-slate-600">(one per line: <code>from -> to</code> or <code>from -> to :overrides_key</code>)</span></label>
    <textarea name="graph_lines" rows="6" placeholder="archaeologist -> prescriber&#10;prescriber -> coder-rust :permit_overrides&#10;coder-rust -> reviewer&#10;reviewer -> shipper" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></textarea></div>
  <div><label class="block text-sm text-slate-400 mb-1">terminal role</label>
    <input name="terminal_role" required placeholder="shipper" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">max retries</label>
    <input name="max_retries" type="number" min="0" value="0" class="w-24 bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100"></div>
  <button type="submit" class="px-4 py-2 rounded bg-indigo-700 hover:bg-indigo-600 text-white">save (auto v=next)</button>
</form>"#;
    Html(page("agentry — new team", body))
}

#[derive(Deserialize)]
struct TeamForm {
    name: String,
    roles_csv: String,
    graph_lines: String,
    terminal_role: String,
    max_retries: u32,
}

fn parse_role_ref(token: &str) -> RoleRef {
    // Accept either "name" (defaults to v1) or "name@vN".
    let trimmed = token.trim();
    if let Some((name, ver)) = trimmed.rsplit_once("@v") {
        if let Ok(version) = ver.parse::<u32>() {
            return RoleRef {
                name: RoleName(name.to_string()),
                version,
            };
        }
    }
    RoleRef {
        name: RoleName(trimmed.to_string()),
        version: 1,
    }
}

fn parse_edge(line: &str) -> Option<MessageEdge> {
    // Format: "from -> to" or "from -> to :overrides_key", with each
    // endpoint optionally version-pinned as "name@vN" (defaults to v1).
    let (edge_part, overrides) = match line.split_once(':') {
        Some((e, rest)) => (e.trim(), Some(rest.trim().to_string())),
        None => (line.trim(), None),
    };
    let (from, to) = edge_part.split_once("->")?;
    Some(MessageEdge {
        from: parse_role_ref(from),
        to: parse_role_ref(to),
        permit_overrides_from: overrides,
        rework_target: None,
    })
}

async fn team_create(
    State(state): State<AppState>,
    Form(f): Form<TeamForm>,
) -> Result<Redirect, AppError> {
    let roles: Vec<RoleRef> = split_csv(&f.roles_csv)
        .iter()
        .map(|s| parse_role_ref(s))
        .collect();
    if roles.is_empty() {
        return Err(AppError(anyhow::anyhow!(
            "team must have at least one role"
        )));
    }
    let edges: Vec<MessageEdge> = split_lines(&f.graph_lines)
        .iter()
        .filter_map(|l| parse_edge(l))
        .collect();

    let version = state.store.next_version("team", &f.name).await?;
    let team = TeamTopology {
        name: TeamName(f.name.clone()),
        version,
        roles,
        message_graph: edges,
        terminal_role: parse_role_ref(&f.terminal_role),
        max_retries: f.max_retries,
    };
    state
        .store
        .save("team", &team.name.0, team.version, &team)
        .await?;
    Ok(Redirect::to("/teams"))
}

// ---------- Projects ----------

async fn projects_list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let items: Vec<(String, Project)> = state.store.list::<Project>("project").await?;
    let mut rows = String::new();
    for (key, p) in &items {
        let slug = p.slug.0.as_str();
        let name = p.name.as_str();
        let default_topo = p
            .default_topology
            .as_ref()
            .map(|t| t.0.as_str())
            .unwrap_or("");
        rows.push_str(&format!(
            r#"<tr class="border-b border-slate-800">
<td class="py-2 font-mono text-sm text-slate-200">{slug}</td>
<td class="py-2 text-sm text-slate-300">{name}</td>
<td class="py-2 text-xs font-mono text-slate-400">{default_topo}</td>
<td class="py-2 text-xs text-slate-600 font-mono">{key}</td>
</tr>"#
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="4" class="py-4 text-slate-500 italic text-sm">No projects yet.</td></tr>"#.into();
    }
    Ok(Html(page(
        "agentry — projects",
        &format!(
            r#"<div class="flex items-center mb-4">
<h2 class="text-lg font-semibold text-slate-200">Projects</h2>
<a href="/projects/new" class="ml-auto px-3 py-1 rounded bg-indigo-700 hover:bg-indigo-600 text-white text-sm">+ new project</a>
</div>
<table class="w-full">
<thead><tr class="text-slate-500 text-xs uppercase tracking-wider border-b border-slate-700">
<th class="text-left py-2">slug</th><th class="text-left py-2">name</th>
<th class="text-left py-2">default topology</th><th class="text-left py-2">redis key</th></tr></thead>
<tbody>{rows}</tbody></table>"#
        ),
    )))
}

async fn project_new_form() -> Html<String> {
    let body = r#"<h2 class="text-lg font-semibold text-slate-200 mb-4">New project</h2>
<form method="POST" action="/projects" class="space-y-4">
  <div><label class="block text-sm text-slate-400 mb-1">slug <span class="text-xs text-slate-600">(lowercase)</span></label>
    <input name="slug" required pattern="[a-z0-9-]+" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">display name</label>
    <input name="name" required class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100"></div>
  <div><label class="block text-sm text-slate-400 mb-1">forges <span class="text-xs text-slate-600">(one per line: forge-slug:owner/repo)</span></label>
    <textarea name="forges_lines" rows="2" required placeholder="agency:yg/qbot-core" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></textarea></div>
  <div class="grid grid-cols-2 gap-4">
    <div><label class="block text-sm text-slate-400 mb-1">default topology</label>
      <input name="default_topology" placeholder="qbot-issue-team" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
    <div><label class="block text-sm text-slate-400 mb-1">steward topology</label>
      <input name="steward_topology" placeholder="qbot-steward" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></div>
  </div>
  <h3 class="text-sm font-semibold text-slate-300 mt-6">Standing orders</h3>
  <div class="grid grid-cols-3 gap-4">
    <div><label class="block text-sm text-slate-400 mb-1">tokens/day</label>
      <input name="tokens_daily" type="number" min="0" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100"></div>
    <div><label class="block text-sm text-slate-400 mb-1">usd/day</label>
      <input name="usd_daily" type="number" step="0.01" min="0" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100"></div>
    <div><label class="block text-sm text-slate-400 mb-1">default escalation</label>
      <select name="default_escalation" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-slate-100">
        <option value="supervised" selected>supervised</option>
        <option value="autonomous">autonomous</option>
        <option value="manual">manual</option>
      </select></div>
  </div>
  <div><label class="block text-sm text-slate-400 mb-1">priorities <span class="text-xs text-slate-600">(one per line)</span></label>
    <textarea name="priorities_lines" rows="3" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-sm text-slate-100"></textarea></div>
  <div><label class="block text-sm text-slate-400 mb-1">forbidden <span class="text-xs text-slate-600">(one per line)</span></label>
    <textarea name="forbidden_lines" rows="3" placeholder="git:force-push:main" class="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 font-mono text-sm text-slate-100"></textarea></div>
  <button type="submit" class="px-4 py-2 rounded bg-indigo-700 hover:bg-indigo-600 text-white">save</button>
</form>"#;
    Html(page("agentry — new project", body))
}

#[derive(Deserialize)]
struct ProjectForm {
    slug: String,
    name: String,
    forges_lines: String,
    default_topology: String,
    steward_topology: String,
    tokens_daily: Option<u64>,
    usd_daily: Option<f64>,
    default_escalation: String,
    priorities_lines: String,
    forbidden_lines: String,
}

async fn project_create(
    State(state): State<AppState>,
    Form(f): Form<ProjectForm>,
) -> Result<Redirect, AppError> {
    let default_escalation: EscalationMode =
        serde_json::from_value(Value::String(f.default_escalation.clone()))
            .map_err(|e| anyhow::anyhow!("invalid default_escalation: {e}"))?;

    let project = Project {
        slug: ProjectSlug(f.slug.clone()),
        name: f.name,
        forges: split_lines(&f.forges_lines),
        default_topology: match f.default_topology.trim() {
            "" => None,
            s => Some(TeamName(s.to_string())),
        },
        steward_topology: match f.steward_topology.trim() {
            "" => None,
            s => Some(TeamName(s.to_string())),
        },
        standing_orders: StandingOrders {
            tokens_daily: f.tokens_daily,
            usd_daily: f.usd_daily,
            default_escalation,
            priorities: split_lines(&f.priorities_lines),
            forbidden: split_lines(&f.forbidden_lines),
        },
        repo_url: None,
        base_branch: None,
        max_concurrent_briefs: None,
    };
    state
        .store
        .save("project", &project.slug.0, 1, &project)
        .await?;
    Ok(Redirect::to("/projects"))
}

// ---------- shell page ----------

fn page(title: &str, body_html: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en" class="dark">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title}</title>
  <script src="https://cdn.tailwindcss.com"></script>
</head>
<body class="bg-slate-950 text-slate-200 font-sans min-h-screen">
  <header class="max-w-5xl mx-auto p-6 border-b border-slate-800 flex items-center gap-6">
    <a href="/" class="text-xl font-semibold text-slate-100">agentry</a>
    <nav class="flex gap-4 text-sm text-slate-400">
      <a href="/" class="hover:text-slate-200">briefs</a>
      <a href="/roles" class="hover:text-slate-200">roles</a>
      <a href="/teams" class="hover:text-slate-200">teams</a>
      <a href="/projects" class="hover:text-slate-200">projects</a>
    </nav>
    <span class="ml-auto text-slate-500 text-xs">M2</span>
  </header>
  <main class="max-w-5xl mx-auto p-6">
    {body_html}
  </main>
</body>
</html>"#
    )
}
