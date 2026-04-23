# Permits

The bounded context that owns *authorization*. A `WorkPermit` is a signed,
time-bounded, scope-bounded credential minted per role per brief. It tells
the spawner what the agent may do (tools, filesystem, network), for how long
(wall-clock, token, USD), and proves its own integrity (ed25519 signature).
Permits are the only safety boundary visible to the spawned container.

## ToolAllowlist

The list of tool symbols a permit authorizes. The permit broker intersects
this with `tool_call` events the agent emits on stdout; an event outside the
list kills the container. Declared on `AgentRole` as a baseline, narrowed
further per-brief via `PermitOverrides`.

## PermitScope

The list of capability scope strings (e.g. `fs:read:/workspace/**`,
`net:allow:api.x.ai`) attached to a permit. Declared on `AgentRole` as a
baseline, narrowed further per-brief via `PermitOverrides`. Today the broker
only enforces `fs:write:*` on literal-path writes; broader enforcement is a
concern of the execution context.

## WorkPermit

The signed permit document: ids for permit and agent, versioned references
to role and brief, allowlist, scope, budget ceilings (max tokens, max wall
seconds, max USD), issue and expiry timestamps, ed25519 signature. Verified
by the spawner immediately before the container is launched; handed to the
container as part of the startup JSON bundle.
