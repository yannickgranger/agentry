# Trace metric

The bounded context that owns *one brief's worth of folded trace
evidence*. Sits downstream of `monitoring` (which keeps a queryable
shadow of the live fleet) and `agent_contract` (which defines the wire
events folded by this context). Producer is `crates/trace-query/`;
two consumer crates (`crates/watchdog-alerts/`,
`crates/trace-rollup-cli/`) ship on day one to ground the Stable
Dependencies Principle even though only the dashboard view (sub-issue
#238) will be wired through later.

The producer's free function `aggregate(brief_id, &mut redis::Connection)`
reads three data sources and folds them best-effort:

- `agentry:brief:{brief_id}:trace` — Redis stream of agent events
  (XRANGE the full range; parse each entry's `event` field as JSON).
- `/transcripts/{brief_id}.jsonl` — the claude transcript on disk
  (contributes `wall_seconds`).
- `agentry:audit:tool-calls:{brief_id}` — Redis list of audit entries
  (contributes `verb_citation_density`; v1 emits 0.0).

Per-source failure (Redis unreachable, transcript missing) degrades to
zero values for the affected fields rather than failing the whole
aggregate. The function returns `Err` only when even the partial-result
path cannot run.

The two consumer crates ship empty entrypoints — `evaluate_thresholds`
returns `Vec::new()`, `rollup` returns `RollupSummary::default()`. They
exist on day one so that producer-side changes to `TraceMetric` are
reviewed against real downstream imports rather than against zero
consumers, grounding the Stable Dependencies Principle. They are NOT
invoked from any production code path.

## TraceMetric

One brief's folded trace evidence. The struct is the published
language between `trace-query` (producer) and any downstream consumer
(`watchdog-alerts`, `trace-rollup-cli`, future dashboard view).

Field set is fixed and additive-only; every field carries
`#[serde(default)]` so a consumer compiled against an older shape
deserialises a newer payload by zero-filling the unknown fields.

- `brief_id: String` — the brief whose trace this metric folds.
- `compile_cycles: u32` — count of `Bash` tool_calls invoking
  `cargo check|build|test|clippy` on the brief's trace stream.
- `reads_before_first_edit: u32` — count of `Read` tool_calls observed
  before the first `Edit`/`Write`/`NotebookEdit` tool_call. Caps at the
  first edit; if no edit ever lands, counts every `Read`.
- `refusal_count: u32` — count of `tool_refused` events plus events
  whose `payload.refused == true` (legacy roles).
- `wall_seconds: u64` — first→last `at` timestamp delta across the
  claude transcript at `/transcripts/{brief_id}.jsonl`, in whole
  seconds. Zero when the transcript is missing or has no parseable
  timestamps.
- `lines_changed: u32` — sum of line-count deltas reported by `Edit` /
  `Write` tool results. v1 emits `0` until tool-result payload shape
  stabilises across claude versions.
- `verb_citation_density: f32` — fraction of brief verbs whose body
  contained a `crate:file:line` citation, computed against the brief
  payload via the tool-call audit log. v1 emits `0.0` until the audit
  log records the brief's verb list.

## RollupSummary

Cross-brief aggregate of `TraceMetric` values, owned by
`crates/trace-rollup-cli/`. Aspirational: today the rollup function
returns `Default` values; real implementation lands when monthly
ratio-over-last-N-briefs becomes a routine operator workflow.

- `mean_compile_cycles: f32` — arithmetic mean of `compile_cycles`
  across the input set.
- `mean_wall_seconds: u64` — arithmetic mean of `wall_seconds` across
  the input set, truncated to whole seconds.
- `mean_lines_changed_per_brief: f32` — arithmetic mean of
  `lines_changed` across the input set.
