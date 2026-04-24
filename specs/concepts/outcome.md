# Outcome

The bounded context that owns *the orchestrator's persisted conclusion about
a brief*. A `Verdict` here is the final record the daemon writes to
`agentry:verdicts` after the team finishes — it is what external systems
(dashboards, webhooks, chain triggers) observe. Distinct from
`agent_contract::EventVerdict`, which is the agent's self-report.

## VerdictKind

The closed set of possible team outcomes: shipped, failed, escalated,
permit-violation. The dashboard and the chain-trigger logic switch on this
kind; external consumers treat it as the brief's published result.

## Verdict

The full verdict record: brief id, kind, timestamp, trace stream reference,
optional human-readable reason. Written to `agentry:verdicts` as an XADD
entry and to the dashboard's SSE feed.
