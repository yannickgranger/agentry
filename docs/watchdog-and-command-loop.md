# Watchdog and command loop

Companion to `specs/concepts/monitoring.md` (typed contract) and
`specs/concepts/briefing.md` (cohort labels). Records the *why* behind
the watchdog and the loop topology it serves. These survive code
refactors, so they belong in repo, not in session memory.

## The loop topology

Three phases. Mechanism is the same; staffing changes.

**Solo (today).** `human → claude (ephemeral chat) → agentry`. Every
brief authored by Claude in a chat session. Every dispatch and every
ship gated by Claude reading agentry output and acting on it.

**Captain (foreseen, not yet built).** `claude (persistent + stdin
daemon overlay) → agentry`, with `human` in the loop only on must-decide
matters. Claude survives between briefs; a stdin daemon feeds new
prompts from substrate events (verdicts, ##STATUS, postmortems).
"Captain" = solo, no staff.

**Commandant (foreseen, not yet built).** Captain promoted: officers
seeded — `architect`, `business`, `code`, `infra` — convened as council
via `TeamCreate` (per global `CLAUDE.md §2b`, never parallel `Agent`
calls). Easy decisions stay in the commandant's loop; hard decisions
summon officer council; only must-decide-human matters escalate.

This document sits at the architecture-of-use level. It does not
prescribe code; it constrains what the code must keep possible.

## Morphology principle

Pipeline morphology will change: squads, multi-project, fan-out cohorts
of up to ~100 mechanical-refactor agents. The watchdog cannot be coupled
to any specific shape. It operates on **agents**, not briefs, not
cohorts, not squads.

The unit of work is the **selector**: a SQL string against the
`monitoring`-owned SQLite agent index. Cohort labels are query
parameters — one dimension among many — not first-class entities. Same
machinery serves 1 agent and 100 agents, 1 project and 50 projects,
without per-shape branching.

## Watchdog model

```
┌─────────────────┐    tick (60s)    ┌──────────────────┐
│  selectors[]    │ ───────────────▶ │  state.db query  │
│  (SQL strings)  │                  └────────┬─────────┘
└─────────────────┘                           │ rows
                                              ▼
                                     ┌──────────────────┐
                                     │  per-agent Grok  │
                                     │  diagnostic      │
                                     └────────┬─────────┘
                                              │ ok | stuck
                                              ▼
                              ┌────────────────────────────────┐
                              │  EventKind::Status { ... }     │
                              │  XADDed to brief trace stream  │
                              └────────────────────────────────┘
```

Tick iterates selectors. Each selector returns a row set. Each row gets
one Grok call. One typed `Status` event per agent. Cost shape is
per-agent, matching the morphology principle.

The watchdog is NOT a captain. It is one signal source the
captain/commandant consumes. Decisions live one layer up.

## Integration points (current code)

| Concern | File:line | Notes |
|---|---|---|
| Watchdog task mount | `crates/orchestrator-runtime/src/daemon.rs:44` | After `tokio::spawn(projector::run(...))`, add a sibling `tokio::spawn(watchdog::run(state.clone(), conn.clone(), cfg))`. |
| Selector query | `crates/orchestrator-runtime/src/state.rs:152` (`State::query`) | Read-only escape hatch already enforced (rejects non-`SELECT`/`WITH`). v1 selector hardcoded; persisted-selectors table is later. |
| ##STATUS event variant | `crates/orchestrator-types/src/event.rs:40` (`EventKind`) | New typed variant `Status { agent_id, ok, stuck, reason, selector_name, evidence_event_ids }`. Touches: `projector.rs:107` (falls through `_` arm to watermark — fine), `delivery.rs:27`/`80` (currently match `Event`/`Finding` with `_` fall-through — fine). Will NOT break existing consumers. |
| Event emission | `crates/orchestrator-runtime/src/spawner.rs:157` (XADD pattern) | Watchdog mirrors this XADD shape against `agentry:brief:{brief_id}:trace`. brief_id obtained from the SQLite row. |
| Cohort label propagation | `crates/orchestrator-types/src/brief.rs:79` (`Brief.cohort_labels`) → spawner → `state.add_cohort_label` | Already wired in F0. Selectors filter on these via `JOIN cohort_labels`. |
| Future officer council mount | (no code yet) | New topology `agentry-council-v0` with roles `officer-architect`, `officer-business`, `officer-code`, `officer-infra`. Same Brief/TeamTopology machinery. Commandant submits a council brief; same dispatch path as today's coder/reviewer/shipper. |
| Future commandant DOL hook | `crates/orchestrator-runtime/src/daemon.rs:494` (`dol_on_brief_terminal`) | Where commandant logic — "retry?", "officer council?", "human escalate?" — would mount, AFTER brief reaches terminal verdict. |
| Future escalation channel | (no code, format TBD) | Forge issue with label / webhook / Redis stream `agentry:escalations` — open question. |

## F-watchdog-1 scope

In:
- One tokio task in `orchestratord` running every 60s (env-overridable).
- One hardcoded selector: `all_running` (`SELECT ... FROM agents WHERE status = 'running'`).
- Per-row Grok-fast HTTPS call with last-N trace events for that
  `agent_id` (live scan of the brief's trace stream, capped at 200
  entries).
- Typed `EventKind::Status` variant.
- XADD one Status event per agent per tick to that agent's brief trace
  stream.

Out (deferred):
- Persisted-selectors table (F-watchdog-3).
- Failed-on-stuck verdict wiring (F-watchdog-2).
- `orchestrator agents list` / `query` CLI (F-cli).
- Agent-trace-tail projector index (F-watchdog-N if scan cost shows up).
- Officer council, escalation channel, commandant promotion gate
  (entire commandant phase — parked).

## Forward-looking (parked)

These are foreseen, not built. Listed here so future code reviews don't
paint into corners that block them.

- **Officer roles + council topology.** Same Brief/TeamTopology
  machinery. New role definitions only.
- **Commandant stdin-daemon overlay.** Persistent Claude Code with
  stable session id; substrate events become piped prompts.
- **Postmortem agent role.** Autonomous failure synthesizer; consumes
  failed verdicts + trace, emits structured analysis.
- **Escalation taxonomy.** When does the commandant escalate to human?
  Open question — must-decide thresholds TBD.
- **Per-squad/project namespacing.** Today there's one Redis keyspace;
  later, multiple parallel projects need stream + topology
  segmentation.
