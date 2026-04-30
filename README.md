# agentry

A self-governing fleet of AI coding agents. The fleet deliberates,
implements, and enforces its own architecture. You set direction; you
shape the work in dialog with the fleet's lead agent; then you stay
out until something genuinely needs you.

## What it is

agentry runs a fleet of AI coding agents. Each agent is a **role** that
does one job inside a short-lived container — produce a change, review
it, commit it, push it, watch CI — and then exits. The fleet itself
decides which roles run in what order (a **workflow**), how failures
route back for fixes, and what the architecture should look like.

Your articulation surface is **/grill-me** — a structured dialog where
the fleet's lead agent (Claude, in the current setup) interviews you
about the work you want done, branch by branch, until you both share
the same understanding of what's being built and why. Once that
understanding is settled, the fleet takes over: a council of specialist
agents deliberates, produces architectural declarations, decomposes the
work into briefs, and dispatches them. agentry runs the briefs, watches
the agents, opens pull requests, watches CI, and either auto-merges on
green or routes failures back to the coder agent for another iteration.

Roles, workflows, and the architectural declarations the fleet enforces
are all stored as data — JSON files in a catalog plus markdown
specifications in `specs/`. New shapes don't require a new release.
The fleet authors and registers them.

## Why it exists

AI coding agents produce code that compiles and looks reasonable but
contains subtle integration bugs: a parameter wired through but read
under the wrong name, a helper duplicated when one already exists, a
feature built layer by layer that doesn't connect. Code review catches
some of this. It misses much, especially when no human reviewer can
match the agent's depth in the language being produced.

agentry's bet: the fleet governs itself. Specialist agents convene as a
council before implementing a feature and produce architectural
declarations (which concepts exist, how they relate, what's banned).
Those declarations are committed to the project as machine-readable
specs and ban rules. CI checks the produced code against them on every
pull request. Drift fails the build. The fleet cannot ship code that
violates an architecture it itself agreed on, even if the code passes
its tests.

You stay outside the loop. You direct (what matters, what's next), you
align with the fleet through /grill-me (the only authoring surface
where your voice enters the work), and you're escalated to only for
decisions that genuinely require human judgment. The fleet handles
authoring, deliberation, implementation, and enforcement.

## What it isn't

- Not a chatbot framework. Agents don't converse — they take a brief,
  do the work, and exit.
- Not a CI runner. Your CI sits underneath; agentry watches it.
- Not opinionated about which AI to use. Roles can wrap Claude, Grok,
  Gemini, plain shell scripts, or compiled binaries — anything that
  reads input and writes structured output.
- Not a single-shot tool. agentry holds the loop: change → review →
  commit → push → CI → on-failure route back to coder, repeat.

## Shape

```mermaid
flowchart LR
    you([You])
    grill[/grill-me dialog/]
    council[(Specialist council<br/>+ specs + ban rules)]
    cat[(Catalog<br/>roles + workflows)]
    sub[agentry]
    agents[Agents in containers]
    pr[Pull request]
    fences{{Architecture checks}}
    merged([Merged])

    you -->|direction| grill
    grill -->|shared understanding| council
    council -->|declarations| cat
    council -->|briefs| sub
    sub -->|read| cat
    sub -->|run one at a time| agents
    agents -->|report back| sub
    agents -->|open| pr
    pr --> fences
    fences -->|pass| merged
    fences -.->|fail, send to coder| sub
    sub -.->|must-decide-human| you
```

The pieces:

1. **/grill-me.** Where you and the fleet's lead agent reach a shared
   understanding of the work. Your voice enters here.
2. **The council.** Specialist agents who deliberate and write the
   architectural declarations the rest of the loop enforces.
3. **The catalog.** Roles, workflows, and specs as data. Authored by
   the council and the lead agent.
4. **agentry itself.** Reads briefs, runs the workflow, watches the
   agents, records outcomes.
5. **The agents.** Short-lived containers, each does one job. Their
   output is the events you see in the dashboard.
6. **The architecture checks.** Run on every pull request. Compare
   what was produced against what the council declared. Block the merge
   on disagreement.

## Try it

```bash
# Local infrastructure (idempotent — safe to re-run).
just dev-redis-up
just agentry-net-up

# Build, generate a signing key, load the starter catalog.
cargo build --release --workspace
./target/release/orchestrator key-gen
./target/release/orchestrator seed

# Run agentry and the dashboard.
./target/release/orchestratord &
./target/release/orchestrator-dashboard &   # http://localhost:7800

# Submit your first brief.
./target/release/orchestrator submit examples/verify-M0.json
```

## Day-to-day commands

```
orchestrator submit <brief.json>           # send work
orchestrator team list / register / show   # manage workflows
orchestrator role list / show              # inspect agents
orchestrator verdicts                      # last N outcomes
orchestrator agents trace <agent-id>       # see what an agent did
agentry-workspace list / gc                # workspace cleanup
```

## Where to read next

- `docs/architectural-control-loop.md` — how a non-trivial feature gets
  designed, agreed on, and turned into briefs. The full loop: /grill-me
  → council → spec → /to-issues → CI fences.
- `docs/dogfood-protocol.md` — what a brief looks like and how to
  dispatch one.
- `specs/concepts/` — the architecture the fleet has declared and the
  CI checker enforces.
- `CLAUDE.md` — house rules for the lead agent (Claude) when working on
  agentry itself.
- `AGENTRY_RESUME.md` — current operational state.
