//! Per-brief trace-metric view. Pull-based: aggregation runs when the
//! handler (or the recent-briefs list-render path) asks for it. Consumes
//! `trace_query::aggregate` — the dashboard does not redefine TraceMetric.

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse};
use orchestrator_types::{Verdict, VerdictKind};
use trace_query::{aggregate, TraceMetric};

use crate::store::DashboardStore;

/// Brief 239 fence: a Shipped verdict that nonetheless carries a non-zero
/// `refusal_count` is anomalous — the coder couldn't reach tools it wanted
/// but produced a passing diff anyway, indicating silent quality routing
/// around denied tools.
#[must_use]
pub fn is_refusal_anomaly(verdict: &Verdict) -> bool {
    matches!(verdict.kind, VerdictKind::Shipped) && verdict.refusal_count > 0
}

/// Render the operator-facing Blocker badge for a refusal-on-shipped
/// anomaly. Returns an empty string when the verdict is not anomalous, so
/// callers can unconditionally splice the result into surrounding markup.
#[must_use]
pub fn refusal_anomaly_badge_html(verdict: &Verdict) -> String {
    if !is_refusal_anomaly(verdict) {
        return String::new();
    }
    let n = verdict.refusal_count;
    format!(
        r#"<span class="anomaly-badge ml-2 px-2 py-0.5 rounded text-xs font-semibold bg-rose-700 text-rose-50" title="This brief shipped with {n} tool-permission refusals — the coder may have produced a passing diff while routed-around denied tools. Review needed.">⚠ refusal-on-shipped anomaly</span>"#
    )
}

/// GET /brief/:brief_id/metrics — render the folded TraceMetric for a
/// brief as a small HTML table. Best-effort: a brief with no trace
/// stream entries renders zeros, not 500.
pub async fn brief_metrics_handler(
    State(store): State<DashboardStore>,
    Path(brief_id): Path<String>,
) -> impl IntoResponse {
    let url = store.redis_url().to_string();
    let bid = brief_id.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<TraceMetric> {
        let client = redis::Client::open(url.as_str())?;
        let mut conn = client.get_connection()?;
        aggregate(&bid, &mut conn)
    })
    .await;

    // Best-effort verdict fetch for the refusal-on-shipped fence (brief
    // 239). A miss (brief not in the recent verdict window, or no verdict
    // yet) just suppresses the badge — never 500s.
    let anomaly_badge = match store.fetch_verdict_for(&brief_id, 100).await {
        Ok(Some(v)) => refusal_anomaly_badge_html(&v),
        _ => String::new(),
    };

    let body = match joined {
        Ok(Ok(metric)) => format!("{}{}", anomaly_badge, render_metrics_table(&metric)),
        Ok(Err(e)) => render_error(&brief_id, &format!("aggregate failed: {e}")),
        Err(e) => render_error(&brief_id, &format!("aggregation task failed: {e}")),
    };
    Html(body)
}

/// Compact one-line badge for inline use in list views (e.g. recent
/// briefs). Renders the two highest-signal counters.
#[must_use]
pub fn metrics_badge_html(metric: &TraceMetric) -> String {
    format!(
        r#"<span class="metric-badge">cycles={} refusals={}</span>"#,
        metric.compile_cycles, metric.refusal_count
    )
}

/// Best-effort metric badge for a brief id, computed by spinning up a
/// short-lived sync redis connection on a blocking thread. Returns an
/// empty string on any failure so list rendering stays fast and never
/// 500s on a missing trace stream.
pub async fn try_badge(redis_url: &str, brief_id: &str) -> String {
    let url = redis_url.to_string();
    let bid = brief_id.to_string();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<TraceMetric> {
        let client = redis::Client::open(url.as_str())?;
        let mut conn = client.get_connection()?;
        aggregate(&bid, &mut conn)
    })
    .await;
    match joined {
        Ok(Ok(metric)) => metrics_badge_html(&metric),
        _ => String::new(),
    }
}

fn render_metrics_table(m: &TraceMetric) -> String {
    let brief_id = html_escape(&m.brief_id);
    format!(
        r#"<table class="metrics text-sm">
<tr><td class="text-slate-400 pr-4">brief_id</td><td class="font-mono">{brief_id}</td></tr>
<tr><td class="text-slate-400 pr-4">compile_cycles</td><td>{cc}</td></tr>
<tr><td class="text-slate-400 pr-4">reads_before_first_edit</td><td>{rbfe}</td></tr>
<tr><td class="text-slate-400 pr-4">refusal_count</td><td>{rc}</td></tr>
<tr><td class="text-slate-400 pr-4">wall_seconds</td><td>{ws}</td></tr>
<tr><td class="text-slate-400 pr-4">lines_changed</td><td>{lc}</td></tr>
<tr><td class="text-slate-400 pr-4">verb_citation_density</td><td>{vcd:.3}</td></tr>
</table>"#,
        cc = m.compile_cycles,
        rbfe = m.reads_before_first_edit,
        rc = m.refusal_count,
        ws = m.wall_seconds,
        lc = m.lines_changed,
        vcd = m.verb_citation_density,
    )
}

fn render_error(brief_id: &str, msg: &str) -> String {
    let brief_id = html_escape(brief_id);
    let msg = html_escape(msg);
    format!(
        r#"<div class="metrics-error text-sm text-rose-300">
<span class="font-mono">{brief_id}</span>: {msg}
</div>"#
    )
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
