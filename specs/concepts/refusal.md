# Refusal (draft)

> Status: **draft**. Ratified when EPIC #231 / #233 wiring lands; today the
> `EventKind::ToolRefused` variant exists with no producers and no
> consumers. See #233a for the types-only intro.

The bounded context that owns *observation of refused tool invocations*.
When Claude's `--allowedTools` fence rejects a tool call, or when the
daemon's permit broker post-hoc audit kills a `tool_call` event for being
outside the role's `ToolAllowlist`, the substrate emits a
`tool_refused` event onto the agent's NDJSON stdout / brief trace stream.
Refusal events let the dashboard, captain, and future officer-council
roles distinguish *the agent tried and was stopped* from *the agent
chose not to act*.

The wire shape lives on `EventKind::ToolRefused { tool, command }` (see
the agent contract): `tool` is the symbolic name of the rejected tool,
`command` is the concrete invocation string when one was available
(e.g. the Bash command line). `command` is `None` for refusals at the
tool-name level (the role is not allowed to call `Read` at all).

Refusal is **not** a verdict. A refused tool is a single event mid-run;
the role keeps running unless it chooses to escalate. The daemon does
not promote a `tool_refused` to a terminal `done failed` automatically —
that policy belongs to the role's prompt and the permit broker's kill
threshold, not to the wire format.
