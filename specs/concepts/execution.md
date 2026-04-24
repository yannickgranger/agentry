# Execution

The bounded context that owns *running work*. The daemon reads briefs off
the queue, resolves their team topology, mints and signs permits, dispatches
each role through a `Spawner`, accumulates messages between roles, and
records outcomes. Execution is the customer of `briefing` + `registry` +
`permits`; it is the supplier of `outcome`.

## AgentStartup

The JSON bundle written to the spawned container's stdin: brief, role,
permit, team context. Published Language between `execution` and the agent's
entrypoint script.

## BriefWorkspace

The per-brief host scratch directory. Allocated by the daemon on the first
role in the team that declares a `WorkspaceMount`, shared across all
subsequent roles in the same brief, torn down on team-level `Shipped` and
retained on any other terminal state for audit. Gives roles a place to
clone, edit, and commit that survives across role boundaries. Minimal
shape today — a host dir bind-mounted at the declared container path;
bare-clone + `git worktree` semantics come later.

## TeamContext

Per-role, per-brief context handed to the container: messages routed to
this role from upstream roles in the same team.

## RoutedMessage

One inter-role message: sender role name, target role name, payload JSON,
timestamp. Accumulated by the daemon as upstream roles ship and filtered
into each downstream role's `TeamContext` at dispatch time.

## AgentHandle

The teardown-facing reference to a spawned container: agent id and
container name. Returned by the `Spawner` so the daemon can reference the
container post-run.

## AgentOutcome

The full post-run record for one role: the handle, the verdict, and the
outbox (messages the role emitted that may feed downstream roles).

## Spawner

The abstract container-lifecycle trait. One method: take a `RunAgentCtx`
bundle and a Redis connection; run the agent to completion; return an
outcome. Substrate-specific implementations specialise this.

## RunAgentCtx

Borrowed bundle of inputs to `Spawner::run_agent` — brief, role, permit,
verifying key, team context, optional brief workspace. Keeps the trait
method signature narrow (two arguments) regardless of how many inputs the
implementation needs to see.

## PodmanSpawner

The concrete spawner backed by rootless Podman. Assembles the `podman run`
command (env passthrough, bind mounts, inline script bootstrap), pipes the
startup bundle to stdin, reads NDJSON events from stdout, enforces the
permit's `ToolAllowlist` and `PermitScope` on every `ToolCall` event,
accumulates the outbox, builds a verdict.

## Error

The runtime's error type. Covers spawn failures, Redis IO failures, permit
signature failures, serialization failures, and configuration errors.
Surfaced at the daemon boundary; never crosses into the container.

## Result

The runtime's `Result` alias bound to `Error`. Used as the return type of
every fallible function inside `orchestrator-runtime`.
