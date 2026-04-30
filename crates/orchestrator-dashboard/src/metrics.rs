//! Per-brief trace-metric view. Pull-based: aggregation runs when the
//! handler (or the recent-briefs list-render path) asks for it. Consumes
//! `trace_query::aggregate` — the dashboard does not redefine TraceMetric.

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse};
use orchestrator_types::{Verdict, VerdictKind};
use trace_query::{aggregate, TraceMetric};

use crate::AppState;

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
    State(state): State<AppState>,
    Path(brief_id): Path<String>,
) -> impl IntoResponse {
    let url = state.store.redis_url().to_string();
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
    let anomaly_badge = match state.store.fetch_verdict_for(&brief_id, 100).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_badge_html_renders_cycles_and_refusals() {
        let m = TraceMetric {
            brief_id: "brf_test".into(),
            compile_cycles: 7,
            refusal_count: 2,
            ..TraceMetric::default()
        };
        let badge = metrics_badge_html(&m);
        assert!(badge.contains("cycles=7"), "badge missing cycles: {badge}");
        assert!(
            badge.contains("refusals=2"),
            "badge missing refusals: {badge}"
        );
        assert!(
            badge.contains("metric-badge"),
            "badge missing class: {badge}"
        );
    }

    #[test]
    fn metrics_badge_html_zero_for_default() {
        let badge = metrics_badge_html(&TraceMetric::default());
        assert!(badge.contains("cycles=0"));
        assert!(badge.contains("refusals=0"));
    }

    #[test]
    fn render_metrics_table_includes_every_field() {
        let m = TraceMetric {
            brief_id: "brf_x".into(),
            compile_cycles: 1,
            reads_before_first_edit: 2,
            refusal_count: 3,
            wall_seconds: 4,
            lines_changed: 5,
            verb_citation_density: 0.5,
        };
        let html = render_metrics_table(&m);
        for needle in [
            "compile_cycles",
            "reads_before_first_edit",
            "refusal_count",
            "wall_seconds",
            "lines_changed",
            "verb_citation_density",
            "brf_x",
        ] {
            assert!(
                html.contains(needle),
                "rendered table missing {needle}: {html}"
            );
        }
    }

    #[test]
    fn html_escape_escapes_dangerous_chars() {
        assert_eq!(
            html_escape("<script>&\"'"),
            "&lt;script&gt;&amp;&quot;&#39;"
        );
    }

    fn synthetic_verdict(kind: VerdictKind, refusal_count: u32) -> Verdict {
        let mut v = Verdict::new(orchestrator_types::BriefId("brf_test".into()), kind);
        v.refusal_count = refusal_count;
        v
    }

    #[test]
    fn is_refusal_anomaly_true_for_shipped_with_refusals() {
        assert!(is_refusal_anomaly(&synthetic_verdict(
            VerdictKind::Shipped,
            3
        )));
    }

    #[test]
    fn is_refusal_anomaly_false_for_failed_with_refusals() {
        assert!(!is_refusal_anomaly(&synthetic_verdict(
            VerdictKind::Failed,
            5
        )));
    }

    #[test]
    fn is_refusal_anomaly_false_for_shipped_without_refusals() {
        assert!(!is_refusal_anomaly(&synthetic_verdict(
            VerdictKind::Shipped,
            0
        )));
    }

    #[test]
    fn refusal_anomaly_badge_html_renders_warning_for_anomaly() {
        let badge = refusal_anomaly_badge_html(&synthetic_verdict(VerdictKind::Shipped, 4));
        assert!(
            badge.contains("⚠ refusal-on-shipped anomaly"),
            "badge missing warning text: {badge}"
        );
        assert!(
            badge.contains("anomaly-badge"),
            "badge missing class: {badge}"
        );
        assert!(
            badge.contains("shipped with 4"),
            "badge missing tooltip count: {badge}"
        );
    }

    #[test]
    fn refusal_anomaly_badge_html_empty_for_non_anomaly() {
        assert!(refusal_anomaly_badge_html(&synthetic_verdict(VerdictKind::Shipped, 0)).is_empty());
        assert!(refusal_anomaly_badge_html(&synthetic_verdict(VerdictKind::Failed, 7)).is_empty());
    }
}
