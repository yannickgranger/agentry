//! Aspirational: when per-brief alerting materializes, this crate evaluates
//! TraceMetric values against operator-configured thresholds and emits alerts.
//!
//! Today exists only to ground SDP for trace-query. Real implementation
//! lands when the alerting use case becomes a regular ask.

use trace_query::TraceMetric;

/// TODO: real threshold evaluation. Today returns no alerts.
pub fn evaluate_thresholds(_metric: &TraceMetric) -> Vec<String> {
    Vec::new()
}
