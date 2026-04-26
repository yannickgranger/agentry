# Agent contract

The bounded context that owns *what a container says on stdout*. Every line
an agent emits is parsed as one `Event`; every event is mirrored to the
brief's trace stream; the terminal event is the one that carries an
`EventVerdict`. This is Published Language between `execution` and every
containerised agent, regardless of substrate or language.

## EventVerdict

Terminal outcome an agent declares in its own stdout: shipped, failed, or
escalated. Distinct from the team-level `outcome::Verdict` — this one
travels on the NDJSON wire; the team-level one is persisted as the brief's
verdict. Kept separate so the agent's self-report cannot silently overwrite
the daemon's reasoned conclusion.

## ToolCall

A tool-invocation attempt emitted by the agent: the tool name and a JSON
args payload. The permit broker checks each one against the permit's
`ToolAllowlist` and, for filesystem writes, against the permit's
`PermitScope`.

## EventKind

The sum type over all event shapes the agent may emit: freeform event,
tool call attempt, inter-role message, log line, done/terminal. Serialised
with a `type` discriminator tag so each NDJSON line is self-describing.

A `status` variant carries a watchdog-emitted diagnosis: the agent id,
the selector that matched it, an `ok`/`stuck` boolean pair, the
diagnostician's natural-language reason, and the trace event ids that
backed the judgment. Watchdog ticks XADD Status events to the agent's
brief trace stream so projector watermarks advance consistently and
downstream consumers (dashboards, captain stdin-daemon, future
commandant officer council) read them on the same wire as every other
agent event.

## Event

A timestamped event: `Ts` + `EventKind`. The unit that the spawner reads
off stdout, writes to the trace stream, and (for `ToolCall`) audits.
