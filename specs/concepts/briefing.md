# Briefing

The bounded context that owns the concept of *work being requested*. A
`Brief` is the unit of work submitted to the orchestrator — it names a team,
carries a payload and a budget, declares how it may escalate, and optionally
points at a parent brief for chain-trigger semantics. Briefing does not know
how work is executed, only how work is described.

## BriefId

Opaque identifier for a single brief. Prefixed (`brf_…`) so it is
distinguishable at a glance from agent ids (`agt_…`) and permit ids
(`prm_…`). Unique across the lifetime of the orchestrator.

## EscalationMode

Declares what the agent team is allowed to do when blocked: stay autonomous,
pause for supervisor input, or escalate immediately. Set per-brief; the
default is the most conservative reading.

## Budget

Per-brief limits: max tokens, max wall-clock seconds, max USD. Lifted into
each role's `WorkPermit` at dispatch time so the spawner and the permit
broker share the same numbers.

## Payload

Free-form JSON attached to the brief. The team's entrypoint role is
responsible for parsing it. Orchestrator does not interpret payload content —
it is Published Language between the submitter and the team.

## Brief

The full record: id, project (optional), topology ref, payload, budget,
escalation mode, parent brief (optional), submitter, submission timestamp.
Persisted on `agentry:briefs` as the XADD entry that starts the workflow.
