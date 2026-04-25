# Outcome

The bounded context that owns *the orchestrator's persisted conclusion about
a brief*. A `Verdict` here is the final record the daemon writes to
`agentry:verdicts` after the team finishes — it is what external systems
(dashboards, webhooks, chain triggers) observe. Distinct from
`agent_contract::EventVerdict`, which is the agent's self-report.

## VerdictKind

The closed set of possible team outcomes: shipped, failed, escalated,
permit-violation, budget-exceeded, aborted, rejected, rework-needed. The
dashboard and the chain-trigger logic switch on this kind; external
consumers treat it as the brief's published result.

The verdict *kind* is a role-level primitive: every role emits one
verdict. The daemon composes per-role verdicts into a team-level
outcome; a team of three roles where the middle role emits
`ReworkNeeded` and the coder then emits `Shipped` on rework still
resolves the team as `Shipped` from the outside view.

## Verdict

The full verdict record: brief id, kind, timestamp, trace stream reference,
optional human-readable reason. Written to `agentry:verdicts` as an XADD
entry and to the dashboard's SSE feed.

The daemon maintains a `Delivery` projection at `agentry:delivery:<brief_id>` (Redis hash) plus an append-only attempts list at `agentry:delivery:<brief_id>:attempts`, derived from trace events. Hash carries pr_number, pr_url, branch, head_sha, ci_state, merged, terminal_kind. Attempts list captures ci_poll, merge_attempt, and rework entries. Untyped Redis access; no pub types.
