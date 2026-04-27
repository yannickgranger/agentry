# Execution

The bounded context that owns *running work*. The daemon reads briefs off
the queue, resolves their team topology, mints and signs permits, dispatches
each role through a `Spawner`, accumulates messages between roles, and
records outcomes. Execution is the customer of `briefing` + `registry` +
`permits`; it is the supplier of `outcome`. When the team's terminal role is
a ci-watcher (as in `agentry-self-host-v0`), the daemon keeps the workspace
alive through the ci-watcher run — though ci-watcher itself does not bind
the workspace, the shipper's preceding push may still be referenced by the
running ci-watcher process. Workspace teardown happens only after the
terminal role's verdict lands.

## AgentStartup

The JSON bundle written to the spawned container's stdin: brief, role,
permit, team context. Published Language between `execution` and the agent's
entrypoint script.

## BriefWorkspace

The per-brief host scratch directory. Allocated by the daemon on the first
role in the team that declares a `WorkspaceMount`, shared across all
subsequent roles in the same brief, torn down on `shipped` /
`review-blocked*` verdicts and retained on every other terminal state for
audit. Gives roles a place to clone, edit, and commit that survives across
role boundaries.

## TerminationDisposition

Two-state verdict-driven disposition for a brief workspace at termination:
`TearDown` (the brief shipped, or its diff lives in a forge PR via a
`review-blocked*` verdict) or `Preserve` (every other failure or unknown
verdict — workspace is kept on disk for forensics until an operator GCs it).
The pure `disposition_for(verdict_str)` function maps the verdict string to
a disposition; the daemon routes through `destroy_with_disposition` so the
two paths share one source of truth.

## WorkspaceEntry

One filesystem record produced by the `agentry-workspace` triage CLI when
walking the briefs root: brief id, host path, age, disk-usage byte count,
and an optional worktree branch hint. Operator-facing data only — the
runtime daemon does not consume it.

## GcTarget

One entry returned by a `gc_run` pass: a `WorkspaceEntry` plus a `removed`
flag indicating whether the dir was actually deleted (always false in
`--dry-run` mode). Lets the CLI report what was reclaimed in a single
structured pass.

When the brief's project (or payload) names a `repo_url` + `base_branch`,
the daemon allocates the workspace as a **git worktree** off a shared
bare clone at `<root>/.clones/<org>/<repo>/`, checked out on branch
`auto/<brief_id>`. Subsequent briefs against the same repo reuse the bare
clone, fetching only what is new. The worktree is removed on team-level
`Shipped`; the bare clone survives.

When no repo is named (probe roles: echo, naughty, speaker, etc.), the
workspace falls back to a plain empty scratch dir — preserving the legacy
semantics for teams that don't need git.

## TeamContext

Per-role, per-brief context handed to the container: messages routed to
this role from upstream roles in the same team.

## RoutedMessage

One inter-role message: sender role name, target role name, payload JSON,
timestamp. Accumulated by the daemon as upstream roles ship and filtered
into each downstream role's `TeamContext` at dispatch time. The ci-watcher
in `agentry-self-host-v0` reads its inputs (`pr_number`, `pr_url`,
`head_sha`) entirely from `TeamContext.messages`; the shipper emits a
Message addressed to the ci-watcher immediately before `emit_done "shipped"`.

The team's `message_graph` defines both message routing AND the execution
DAG. Roles with no inbound edges fire first; roles with multiple inbound
edges fire when ALL their upstreams have shipped (the join). Sibling roles
with identical upstream sets run concurrently via `tokio::join!`. The
daemon's sequential iteration is gone; rework still rewinds to the single
upstream named by `team.incoming(role).first()`, but the downstream
sub-DAG re-enters the pending state on rework and re-fires when upstream
ships again. Concurrent roles share the brief workspace as readers; the
DAG must be authored so that at most one parallel role mutates the
workspace at a time.

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
accumulates the outbox, builds a verdict. When a role declares
`exitpoint_script`, the `PodmanSpawner` bootstrap chains it after the
entrypoint: `bash -c $AGENTRY_SCRIPT && exec bash -c $AGENTRY_EXITPOINT`.
The entrypoint's early-error paths short-circuit via `exit 0` after
emitting `done`; the exitpoint runs regardless (its output is safely
discarded post-terminal) unless the entrypoint exits non-zero. This
gives every role a substrate-agnostic place for post-worker gates
without forking the script surface.

## Error

The runtime's error type. Covers spawn failures, Redis IO failures, permit
signature failures, serialization failures, and configuration errors.
Surfaced at the daemon boundary; never crosses into the container.

## Result

The runtime's `Result` alias bound to `Error`. Used as the return type of
every fallible function inside `orchestrator-runtime`.

_marker: 2026-04-25-poc_
