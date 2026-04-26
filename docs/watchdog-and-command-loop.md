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

## Observability surfaces

The watchdog and the rest of the runtime produce a small set of
log-shaped artifacts. Together they are the primary record of what the
fleet is doing; every operator question ("is the agent stuck?",
"what did the coder actually run?", "which brief failed and why?")
resolves against one of these.

| Surface | Where | What it carries |
|---|---|---|
| Trace stream | Redis stream `agentry:brief:<id>:trace` | Every agent event ordered: spawn, all NDJSON the agent emitted, watchdog `Status`, `watchdog_kill` annotation, terminate. The primary log. |
| Agent index | SQLite at `~/.config/agentry/state.db` (path overridable via `AGENTRY_STATE_PATH`) | One row per agent: id, brief, role, project, started/last-event timestamps, status, verdict, exit code, cohort labels. |
| Verdicts stream | Redis stream `agentry:verdicts` | Per-brief final outcome (Shipped, Failed, ReworkNeeded, PermitViolation, Escalated, Rejected). |
| Brief workspace | Filesystem `/var/mnt/workspaces/agentry-work/briefs/<brief_id>/` | The git worktree the coder used. Preserved on team-failure (`daemon.rs:399-407`); destroyed on team-success. |
| Daemon stdout | Currently ad-hoc `/tmp/orchestratord-*.log` (nohup) | tracing-subscriber output: brief receives, role completed, projector/watchdog warns. **Gap — see below.** |

### Reading the trace stream

Follow one brief from spawn to ship:

```bash
redis-cli -p 6380 -a "$(cat ~/.config/agentry/redis.password)" \
    XRANGE agentry:brief:<brief_id>:trace - +
```

Follow one agent across that brief (filter the stream by the `agent`
field client-side):

```python
import redis, json, os
r = redis.Redis(host="127.0.0.1", port=6380,
                password=open(os.path.expanduser("~/.config/agentry/redis.password")).read().strip(),
                decode_responses=True)
for sid, fields in r.xrange("agentry:brief:<brief_id>:trace", count=500):
    if fields.get("agent", "").startswith("<agent_id_prefix>"):
        print(sid, json.loads(fields["event"]))
```

Extract just the watchdog's verdicts on one agent:

```python
for sid, fields in r.xrange("agentry:brief:<brief_id>:trace", count=500):
    ev = json.loads(fields["event"])
    if ev.get("type") == "status":
        print(sid, ev["agent_id"], "ok=", ev["ok"], "reason=", ev["reason"])
```

### Reading the agent index

The SQLite store accepts SELECT/WITH only via `State::query`; in the
shell, use the read-only `sqlite3` CLI (or `python3 sqlite3`):

```bash
sqlite3 -readonly ~/.config/agentry/state.db \
    "SELECT agent_id, role_name, status, verdict FROM agents \
     WHERE brief_id = 'brf_work_…' ORDER BY started_at"
```

Fleet-wide selectors are the same shape the watchdog already runs
internally — the `all_running` selector is just
`SELECT … FROM agents WHERE status = 'running'`.

### Stuck-agent forensics recipe

When the watchdog kills an agent, the audit trail in the trace stream
is a fixed sequence:

1. N consecutive `EventKind::Status { stuck: true, reason: "…", … }`
   events, where N is `stuck_threshold` (default 3, env
   `AGENTRY_WATCHDOG__STUCK_THRESHOLD`). Each carries `evidence_event_ids`
   pointing to the trace entries that drove Grok's judgment.
2. One `EventKind::Event { payload: { agent_event: "watchdog_kill",
   consecutive_stuck: N, reason: "…" } }` annotation marking the kill.
3. Container exit (`podman kill agentry-<agent_id>`) → spawner's stdout
   reader sees EOF without a `Done` event → `compute_verdict` returns
   `(VerdictKind::Failed, Some("agent exited without done event"))`.
4. The brief's team-shipped flag flips false → `daemon.rs:399-407`
   preserves the workspace at
   `/var/mnt/workspaces/agentry-work/briefs/<brief_id>/` for
   post-mortem inspection.
5. `agentry:verdicts` records the brief-level Failed outcome.

To investigate: pull the trace stream filtered by the agent id, follow
the Status reasons backward to the last `ok=true`, and inspect the
workspace for the actual files the agent was working on at the time.

## Observability gaps

Flagged here so future briefs don't paint into corners:

- **No CLI yet.** Today, every query above is via `redis-cli` /
  `python3 redis` / `sqlite3`. F-cli will land
  `orchestrator agents list / query <SQL> / trace <agent_id> /
  recent-status <agent_id>` so the same answers are one command away.
- **Container `--rm` means the trace stream is the only persistence
  path** for an agent's runtime output. Anything an agent script writes
  to bash stderr without going through the BASH_PRELUDE `emit_event`
  helper is lost when the container exits. Currently fine because every
  prelude helper structurally emits events, but worth knowing.
- **Daemon stdout is ad-hoc.** `nohup ./target/release/orchestratord >
  /tmp/orchestratord.log 2>&1 &` is the current pattern. No rotation,
  no journald hookup, lost on tmpfs flush. Becomes load-bearing when
  agentry runs on a server we don't babysit.
- **No retention policy.** Redis streams grow forever (`XADD *` only
  appends). At current scale this is invisible; eventually it needs an
  XTRIM / MAXLEN policy or we run out of memory.
