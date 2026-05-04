# Allowed tools

> Status: **ratified**. Code landing PR: #233b-1. The type is wired into
> `AgentRole.allowed_tools` and propagated through `mint_permit` into
> `WorkPermit.allowed_tools`. The `stream_claude` consumer (typed
> `emit_tool_refused` + `parse_tool_refusal`, lifting `parse_allowed_tools`)
> lands in #247 and remains gated by `refusal.md`'s draft status.

The bounded context that owns *pre-spawn tool fencing for Claude-CLI roles*.
A role spawns the `claude` binary with `--allowedTools <pattern>...`; the
patterns name Claude tools (`Bash(cargo fmt:*)`, `Read`, `Edit(*.rs)`) and
follow Claude's own pattern grammar. The fence runs inside the Claude
process before any tool can fire — violations never reach the daemon.

This is intentionally distinct from the daemon-side `ToolAllowlist` in the
`permits` context: that one carries exact-match symbolic names (`bash`,
`read`, `edit`) and is enforced post-hoc by the permit broker against
`tool_call` events. The two grammars live in two different value domains
and are NOT auto-synchronised — they enforce at different layers, against
different vocabularies, with different failure shapes (Claude refusal vs.
daemon kill).

## AllowedTools

The list of `claude --allowedTools` pattern strings a role declares for its
spawned Claude process. Open-ended grammar — patterns are passed through to
the Claude CLI verbatim. Carried as a `Vec<String>` newtype with serde
round-trip; consumers (role spec field, `stream_claude` arg-builder,
refusal observer) land in #233 wiring.
