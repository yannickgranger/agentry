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

## Event

A timestamped event: `Ts` + `EventKind`. The unit that the spawner reads
off stdout, writes to the trace stream, and (for `ToolCall`) audits.
