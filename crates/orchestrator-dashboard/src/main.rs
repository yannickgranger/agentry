//! orchestrator-dashboard — live view over Redis state.
//!
//! Single-file dashboard (M1): Axum + SSE + hand-rolled HTML + Tailwind CDN.
//! Read-only; no writes (the registry editor lands in M2).
//!
//! Routes:
//!   GET /                           index (recent verdicts + active briefs)
//!   GET /brief/:id                  brief detail with live trace
//!   GET /sse/verdicts               SSE stream of new verdicts
//!   GET /sse/brief/:id/trace        SSE stream of brief's trace events
//!   GET /healthz                    liveness
//!
//! The SSE handlers tail Redis streams; the HTML bootstraps the latest entries
//! and the EventSource live-appends as new entries arrive.

#![forbid(unsafe_code)]

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::Stream;
use orchestrator_runtime::redis_io;
use redis::aio::ConnectionManager;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use serde_json::Value;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const STREAM_BRIEFS: &str = "agentry:briefs";

#[derive(Clone)]
struct AppState {
    redis: Arc<tokio::sync::Mutex<ConnectionManager>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchestrator_dashboard=info,info".into()),
        )
        .init();

    let port: u16 = std::env::var("AGENTRY_DASHBOARD_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7800);

    let conn = redis_io::connect()
        .await
        .map_err(|e| anyhow::anyhow!("redis connect: {e}"))?;
    let state = AppState {
        redis: Arc::new(tokio::sync::Mutex::new(conn)),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/brief/{id}", get(brief_detail))
        .route("/sse/verdicts", get(sse_verdicts))
        .route("/sse/brief/{id}/trace", get(sse_brief_trace))
        .route("/healthz", get(healthz))
        .with_state(state);

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
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
        (StatusCode::INTERNAL_SERVER_ERROR, format!("error: {}", self.0)).into_response()
    }
}

// ---------- index ----------

async fn index(State(app): State<AppState>) -> Result<Html<String>, AppError> {
    let (verdicts, briefs) = {
        let mut c = app.redis.lock().await;
        let v = fetch_recent_verdicts(&mut c, 20).await?;
        let b = fetch_recent_briefs(&mut c, 20).await?;
        (v, b)
    };

    let verdict_ids: std::collections::HashSet<String> =
        verdicts.iter().filter_map(|v| v.get("brief").and_then(Value::as_str).map(String::from)).collect();

    let mut active_items = String::new();
    for b in briefs.iter().rev() {
        let brief_id = b.get("id").and_then(Value::as_str).unwrap_or("?");
        if verdict_ids.contains(brief_id) {
            continue;
        }
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
</li>"#
        ));
    }
    if active_items.is_empty() {
        active_items.push_str(r#"<li class="text-slate-500 italic text-sm">No briefs in flight.</li>"#);
    }

    let mut verdict_items = String::new();
    for v in &verdicts {
        verdict_items.push_str(&render_verdict_li(v));
    }
    if verdict_items.is_empty() {
        verdict_items
            .push_str(r#"<li class="text-slate-500 italic text-sm">No verdicts yet.</li>"#);
    }

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
  <ul id="verdicts">{verdict_items}</ul>
</section>

<script>
const es = new EventSource("/sse/verdicts");
es.addEventListener("verdict", (e) => {{
    const v = JSON.parse(e.data);
    const html = `<li class="py-1 border-b border-slate-800 last:border-0">
<a class="text-indigo-300 hover:text-indigo-200 font-mono text-sm" href="/brief/${{v.brief}}">${{v.brief}}</a>
<span class="mx-2 px-2 py-0.5 rounded text-xs ${{v.kind === 'shipped' ? 'bg-emerald-900 text-emerald-200' : 'bg-rose-900 text-rose-200'}}">${{v.kind}}</span>
<span class="text-slate-500 text-xs">${{v.at}}</span></li>`;
    const list = document.getElementById("verdicts");
    // Remove the "no verdicts" placeholder if present.
    if (list.querySelector("li.italic")) list.innerHTML = "";
    list.insertAdjacentHTML("afterbegin", html);
}});
es.onerror = () => {{ /* auto-reconnect */ }};
</script>
"#
        ),
    )))
}

fn render_verdict_li(v: &Value) -> String {
    let brief = v.get("brief").and_then(Value::as_str).unwrap_or("?");
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("?");
    let at = v.get("at").and_then(Value::as_str).unwrap_or("");
    let badge = match kind {
        "shipped" => "bg-emerald-900 text-emerald-200",
        "failed" | "permit_violation" | "budget_exceeded" | "aborted" => "bg-rose-900 text-rose-200",
        "escalated" => "bg-amber-900 text-amber-200",
        _ => "bg-slate-800 text-slate-200",
    };
    format!(
        r#"<li class="py-1 border-b border-slate-800 last:border-0">
<a class="text-indigo-300 hover:text-indigo-200 font-mono text-sm" href="/brief/{brief}">{brief}</a>
<span class="mx-2 px-2 py-0.5 rounded text-xs {badge}">{kind}</span>
<span class="text-slate-500 text-xs">{at}</span>
</li>"#
    )
}

// ---------- brief detail ----------

async fn brief_detail(
    Path(id): Path<String>,
    State(app): State<AppState>,
) -> Result<Html<String>, AppError> {
    let existing = {
        let mut c = app.redis.lock().await;
        fetch_trace_history(&mut c, &id, 200).await?
    };

    let mut events_html = String::new();
    for ev in &existing {
        events_html.push_str(&render_trace_li(ev));
    }
    if events_html.is_empty() {
        events_html
            .push_str(r#"<li class="text-slate-500 italic text-sm">No events yet.</li>"#);
    }

    let body = format!(
        r#"
<p class="mb-4"><a class="text-indigo-300 hover:text-indigo-200" href="/">&larr; back</a></p>

<h2 class="text-lg font-semibold text-slate-200 mb-2">Brief <span class="font-mono">{id}</span></h2>

<ul id="trace" class="space-y-1 text-sm">{events_html}</ul>

<script>
const es = new EventSource("/sse/brief/{id}/trace");
es.addEventListener("event", (e) => {{
    const data = JSON.parse(e.data);
    const body = document.getElementById("trace");
    if (body.querySelector("li.italic")) body.innerHTML = "";
    body.insertAdjacentHTML("beforeend", renderEvent(data));
    window.scrollTo(0, document.body.scrollHeight);
}});
es.onerror = () => {{ /* auto-reconnect */ }};

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
</script>
"#
    );

    Ok(Html(page(&format!("agentry — {id}"), &body)))
}

fn render_trace_li(ev: &Value) -> String {
    let ty = ev.get("type").and_then(Value::as_str).unwrap_or("?");
    let at = ev.get("at").and_then(Value::as_str).unwrap_or("");
    let (pill, detail) = match ty {
        "done" => {
            let verdict = ev.get("verdict").and_then(Value::as_str).unwrap_or("?");
            let cls = if verdict == "shipped" {
                "bg-emerald-900 text-emerald-200"
            } else {
                "bg-rose-900 text-rose-200"
            };
            (cls, format!("verdict=<b>{verdict}</b>"))
        }
        "tool_call" => {
            let call = ev.get("call");
            let tool = call
                .and_then(|c| c.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            let args = call.and_then(|c| c.get("args")).cloned().unwrap_or(Value::Null);
            (
                "bg-indigo-900 text-indigo-200",
                format!("{tool} {}", serde_json::to_string(&args).unwrap_or_default()),
            )
        }
        "message" => {
            let to = ev.get("to").and_then(Value::as_str).unwrap_or("?");
            let payload = ev.get("payload").cloned().unwrap_or(Value::Null);
            (
                "bg-cyan-900 text-cyan-200",
                format!("to={to} {}", serde_json::to_string(&payload).unwrap_or_default()),
            )
        }
        "log" => {
            let lvl = ev.get("level").and_then(Value::as_str).unwrap_or("info");
            let msg = ev.get("msg").and_then(Value::as_str).unwrap_or("");
            ("bg-slate-800 text-slate-400", format!("[{lvl}] {msg}"))
        }
        _ => {
            let payload = ev.get("payload").cloned().unwrap_or(Value::Null);
            (
                "bg-slate-700 text-slate-200",
                serde_json::to_string(&payload).unwrap_or_default(),
            )
        }
    };
    format!(
        r#"<li class="py-1 border-b border-slate-800 last:border-0">
<span class="text-slate-500 font-mono text-xs">{at}</span>
<span class="mx-2 px-2 py-0.5 rounded text-xs {pill}">{ty}</span>
<span class="text-slate-300">{detail}</span>
</li>"#
    )
}

// ---------- SSE ----------

async fn sse_verdicts(
    State(app): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);
    let redis = app.redis.clone();
    tokio::spawn(async move {
        tail_stream(redis, "agentry:verdicts", "verdict", "verdict", tx).await;
    });
    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn sse_brief_trace(
    Path(id): Path<String>,
    State(app): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = format!("agentry:brief:{id}:trace");
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);
    let redis = app.redis.clone();
    tokio::spawn(async move {
        tail_stream(redis, Box::leak(stream.into_boxed_str()), "event", "event", tx).await;
    });
    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Tail a Redis stream from `$`, forwarding entries whose map-key `field` is a JSON
/// body. Each forwarded SSE event uses `sse_event_name` as its event type.
async fn tail_stream(
    redis: Arc<tokio::sync::Mutex<ConnectionManager>>,
    stream: &'static str,
    field: &'static str,
    sse_event_name: &'static str,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    let mut last_id = "$".to_string();
    loop {
        let read: Result<Option<StreamReadReply>, redis::RedisError> = {
            let mut c = redis.lock().await;
            let opts = StreamReadOptions::default()
                .block(5_000)
                .count(16);
            c.xread_options(&[stream], &[&last_id], &opts).await
        };
        match read {
            Ok(Some(reply)) => {
                for k in reply.keys {
                    for entry in k.ids {
                        last_id = entry.id.clone();
                        if let Some(body) = entry.map.get(field).and_then(redis_value_to_str) {
                            let ev = Event::default().event(sse_event_name).data(body);
                            if tx.send(Ok(ev)).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(error=%err, stream=%stream, "xread error");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

// ---------- fetch helpers ----------

async fn fetch_recent_verdicts(
    conn: &mut ConnectionManager,
    count: usize,
) -> anyhow::Result<Vec<Value>> {
    let reply: redis::streams::StreamRangeReply =
        conn.xrevrange_count("agentry:verdicts", "+", "-", count).await?;
    let mut out = Vec::with_capacity(reply.ids.len());
    for entry in reply.ids {
        if let Some(body) = entry.map.get("verdict").and_then(redis_value_to_str) {
            if let Ok(v) = serde_json::from_str(&body) {
                out.push(v);
            }
        }
    }
    Ok(out)
}

async fn fetch_recent_briefs(
    conn: &mut ConnectionManager,
    count: usize,
) -> anyhow::Result<Vec<Value>> {
    let reply: redis::streams::StreamRangeReply =
        conn.xrevrange_count(STREAM_BRIEFS, "+", "-", count).await?;
    let mut out = Vec::with_capacity(reply.ids.len());
    for entry in reply.ids {
        if let Some(body) = entry.map.get("brief").and_then(redis_value_to_str) {
            if let Ok(v) = serde_json::from_str(&body) {
                out.push(v);
            }
        }
    }
    Ok(out)
}

async fn fetch_trace_history(
    conn: &mut ConnectionManager,
    brief_id: &str,
    count: usize,
) -> anyhow::Result<Vec<Value>> {
    let stream = format!("agentry:brief:{brief_id}:trace");
    let reply: redis::streams::StreamRangeReply =
        conn.xrange_count(&stream, "-", "+", count).await?;
    let mut out = Vec::with_capacity(reply.ids.len());
    for entry in reply.ids {
        if let Some(body) = entry.map.get("event").and_then(redis_value_to_str) {
            if let Ok(v) = serde_json::from_str(&body) {
                out.push(v);
            }
        }
    }
    Ok(out)
}

fn redis_value_to_str(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
        redis::Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
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
  <header class="max-w-4xl mx-auto p-6 border-b border-slate-800 flex items-center">
    <a href="/" class="text-xl font-semibold text-slate-100">agentry</a>
    <span class="ml-3 text-slate-500 text-sm">ephemeral agent orchestrator</span>
    <span class="ml-auto text-slate-500 text-xs">M1 · read-only</span>
  </header>
  <main class="max-w-4xl mx-auto p-6">
    {body_html}
  </main>
</body>
</html>"#
    )
}
