# Monitoring

The bounded context that owns *observation of the running fleet*. While
`execution` runs work and `outcome` records the verdict, `monitoring` keeps
a queryable shadow of which agents are alive, what they are doing right now,
and which cohorts they belong to. The shadow is a SQLite store materialized
by a projector task that consumes brief trace streams and folds spawn,
heartbeat, and termination events into typed rows. Monitoring is the
customer of `execution`'s trace stream and `briefing`'s `cohort_labels`; it
is the supplier of fleet state to dashboards, watchdogs, and selectors that
address subsets of the fleet by cohort.

## State

The agent-state SQLite store. Owns the schema (the `agents` table joined
to `cohort_labels` on agent id) and the operations that maintain it:
upsert on spawn, update on heartbeat, mark-terminated on the agent's
final verdict, attach cohort labels. State also exposes a read-only
`query` escape hatch — any SQL whose first whitespace-trimmed token isn't
`SELECT` or `WITH` is rejected — so future monitoring layers can express
selectors as arbitrary SQL without the store handing out write access.
The store is opened once at daemon startup (path overridable via the
`AGENTRY_STATE_PATH` env, defaulting under `$HOME/.config/agentry/`) and
shared across the daemon by Arc.

The store is also exposed via the `orchestrator agents` CLI for ad-hoc
operator queries (`agents list`, `agents query <sql>`); the same
SELECT/WITH guard applies to CLI passthru.

## AgentRow

The materialized record for one agent: id, brief id, role name, optional
project slug, started-at and last-event-at timestamps, status (`running`
or `terminated`), optional verdict and exit code, and the list of cohort
labels propagated from the brief that spawned the agent. AgentRow is the
unit returned by typed reads against the store; the rows fall out of the
projector folding `agent_event=spawned` and `agent_event=terminated`
events emitted by the spawner around the container's lifecycle. Watchdog
and Grok-diagnostic layers in subsequent briefs read AgentRow snapshots
to decide whether an agent looks stuck and what to do about it.

## Selector

A named read-only SQL query against the agent index. The watchdog tick
iterates registered Selectors; each row in a Selector's result becomes
one Grok diagnostic call and one Status event. Selectors are how
monitoring addresses "which agents to look at" without coupling to
brief, cohort, or squad shape — any subset expressible as SQL against
`agents` and `cohort_labels` is reachable. v1 ships a single hardcoded
Selector (`all_running`); persisted-Selector storage is a later brief.
The Selector's `sql` string flows through `State::query`'s SELECT/WITH
guard, so write access is structurally impossible from this path.

## Watchdog

The long-running task that consumes the agent index through registered
Selectors, diagnoses each selected agent via an external LLM probe
(xAI Grok-fast), and emits one typed Status event per agent per tick.
The Watchdog runs alongside the projector inside the daemon. Without an
`XAI_API_KEY` in the daemon environment, the daemon does not spawn the
Watchdog at all — diagnostics are an optional augmentation, never a
hard requirement for brief execution. Tick cadence, Grok endpoint, and
Grok model name are env-overridable via `AGENTRY_WATCHDOG__TICK_SECONDS`,
`AGENTRY_WATCHDOG__GROK_API_URL`, `AGENTRY_WATCHDOG__GROK_MODEL`. Per-
agent failures (Grok HTTP, malformed JSON, evidence-scan errors) are
logged and skipped — the Watchdog never crashes the daemon nor blocks
brief execution.

Beyond observation, the Watchdog escalates: after `stuck_threshold`
consecutive `stuck=true` Status verdicts on the same agent (default 3,
overridable via `AGENTRY_WATCHDOG__STUCK_THRESHOLD`), it XADDs a
`watchdog_kill` annotation event to the agent's brief trace stream and
issues `podman kill` against the agent's container. The spawner's
stdout reader then sees EOF without a Done event, and its
`compute_verdict` fall-through emits a Failed verdict naturally; the
daemon's team-failure path preserves the workspace for audit. The
counter resets when an `ok=true` verdict arrives, so a transient stall
followed by recovery does not escalate. Counters live in memory only;
daemon restart resets them and the next tick begins fresh.

Kill escalation is also gated on the *distinct-payload count* of the
recent evidence tail. If `stuck_threshold` consecutive `stuck=true`
verdicts arrive but the tail consists of fewer than
`distinct_payload_threshold` (default 2, env-overridable via
`AGENTRY_WATCHDOG__DISTINCT_PAYLOAD_THRESHOLD`) distinct payload
bodies, the Watchdog logs at debug and skips the kill. This
defends against false positives on legitimate long-poll-loop
agents (e.g. ci-watcher) whose tail is the same payload repeating
but whose work is healthy. The consecutive-stuck counter still
increments, so an agent that subsequently emits variety while
remaining stuck does escalate correctly.

## TranscriptEvent

One parsed line from a `claude -p --output-format stream-json --verbose`
transcript: `SystemInit`, `Assistant`, `User`, `Result`, or `Other`
(unknown event kinds preserved verbatim). The transcript module is pure —
callers feed it the JSONL string read from
`/var/lib/agentry/transcripts/<brief>[.<role>].jsonl` and receive typed
events with no I/O. Mid-stream truncation (the `timeout`-kill case) is
tolerated: a partial trailing line is dropped silently.

## ToolUse

One `tool_use` block extracted from an assistant turn: id, tool name,
input JSON, parse-time started_at. Drives the `tool_histogram` in
`TranscriptSummary` and the `tool` field in `LastToolCall`.

## ToolResult

One `tool_result` block from a user turn paired with its `ToolUse` by
`tool_use_id`. Distinguishes a still-running tool call (no matching
result yet) from a completed one in `LastToolCall.completed`.

## LastToolCall

The transcript's most recent tool invocation, projected for the
dashboard's "what is this agent doing right now" view: tool name, input
JSON, started_at, duration so far in seconds, and a `completed` flag.
Returned by `GET /briefs/{id}/transcript/last-tool-call`.

## TranscriptSummary

Aggregate stats over a transcript: tool histogram, token totals, wall
clock, event count, and first/last timestamps. Cheap to compute on every
request because transcripts are small. Returned by
`GET /briefs/{id}/transcript/summary`.

## BriefsState

The dashboard's brief-routes state: the transcripts root directory under
which `<brief>[.<role>].jsonl` files are read. Constructed with a
production default of `/var/lib/agentry/transcripts` and overridable in
integration tests so the routes can be exercised against a tempdir
without touching the host filesystem.
