//! Aspirational: monthly batch CLI that aggregates TraceMetric values
//! across briefs to compute the ratio code-produced / time-consumed
//! at scope wider than one brief.
//!
//! Today exists only to ground SDP. Real implementation lands when
//! manual rollup of ratio-over-last-20-briefs becomes routine.

use trace_query::TraceMetric;

#[derive(Default)]
pub struct RollupSummary {
    pub mean_compile_cycles: f32,
    pub mean_wall_seconds: u64,
    pub mean_lines_changed_per_brief: f32,
}

/// TODO: real aggregation. Today returns Default values.
pub fn rollup(_metrics: &[TraceMetric]) -> RollupSummary {
    RollupSummary::default()
}
