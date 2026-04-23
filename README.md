# agentry

Minimal orchestrator for ephemeral agent containers. Methodology as data, enforcement as physics.

## What it does

Accepts a `Brief` on a Redis stream. Looks up the `TeamTopology` the brief references. For each `AgentRole` in the topology, spawns a fresh container on a user-chosen substrate (podman by default), with a **capability-minimized** toolset and a signed `WorkPermit`. Routes messages between agents per the topology's graph. Monitors every tool call against the permit. Tears down all containers when the team emits `done`.

The orchestrator doesn't know what "TDD", "gate", or "review" mean. That's what team topologies encode.

## Quickstart

```bash
# Dev infra up (requires podman + local systemd user + Redis reachable)
just dev-up

# Submit a brief
orchestrator submit examples/verify-M0.json

# See it
open http://localhost:7800

# Tear down
just dev-down
```

## Shape

- `Brief` — unit of work.
- `AgentRole` — container spec (tools, model, substrate).
- `TeamTopology` — roles + message graph + permit-override rules.
- `Project` — scoping record (budget, escalation, default topology).

All four records live in Redis, typed, edited via dashboard forms.

**No YAML files. No skills. No gates in the runtime.** Just a bus driver.

## Crates

- `orchestrator-types` — pure types, serde.
- `orchestrator-runtime` — daemon, Redis consumer, Spawner, permit broker, CLI.
- `orchestrator-dashboard` — Axum + htmx + SSE.

## Design docs

- **`AGENTRY_RESUME.md`** — session-portable resume plan (canonical).
- **`docs/PROPOSAL.md`** — full proposal from archaeology synthesis (2026-04-23).
- **`docs/archaeology/`** — mining artifacts (KB, Redis, v1 graveyard post-mortem, code gems).

## Status

Pre-M0 scaffolding. See `TODO.md` for the next concrete action.
