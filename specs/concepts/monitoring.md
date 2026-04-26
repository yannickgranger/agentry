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
