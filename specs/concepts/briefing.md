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

#### Validator-dispatch kind (legacy)

The `Brief.kind` field on `Brief` (`brief.rs::BriefKind`) selects which
validator pipeline runs against the candidate diff before the daemon
accepts it. Variants name the work shape, not the team or topology:
`Refactor`, `Debug`, `Mechanical`, `NewFeature`, `Substrate`, `Audit`,
`Doc`. Optional on `Brief` for backwards compatibility — briefs
submitted before the field existed deserialize with `kind: None`. The
`validators::registry_for` function maps each variant to a concrete
validator pipeline; nothing else interprets the field. The newer
task-shape classification (`kind.rs::BriefKind`, see
`specs/concepts/brief_kind.md`) shares the bare identifier; graph-specs
treats them as one concept.

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

A brief may also carry `cohort_labels`: an optional list of free-form strings
propagated to every agent the brief spawns. Set by the dispatching authority
(captain, officer, or human submitter); the orchestrator does not assign or
interpret them. Monitoring selectors use cohort labels to address subsets of
the agent fleet — "every coder in the self-host topology", "every agent in
phase X" — without the orchestrator having to know what those subsets mean.

A brief may also carry `redeploy_required`: a list of `RedeployTarget` values
indicating which binaries must be rebuilt after the brief merges. Empty (and
omitted on the wire) for briefs that don't touch redeployable code.

## RedeployTarget

Names a binary that must be redeployed after a brief merges. Variants:
`Daemon`, `OrchestratorCli`, `CaptainCli`. Carried on `Brief.redeploy_required`
as data only — F8a defines the schema; the captain CLI's `redeploy` subcommand
(F8b) is what reads the field and runs the rebuild. Wire form is snake_case
(`daemon`, `orchestrator_cli`, `captain_cli`).

## Bundle

Stdin envelope a Rust role binary reads when the orchestrator dispatches a
brief to it. Single-field wrapper around `Brief` — the binary parses
`{"brief": { ... }}` so the on-wire shape stays stable when future fields
(e.g. team-level overrides) join the bundle. Used today by the
git-operator family (`git-op-commit`, `git-op-push`, the legacy combined
`git-operator`); other Rust ports will adopt the same envelope as they
land under EPIC #161.
