# Registry

The bounded context that owns the *catalog of what teams and agents exist*.
Roles, teams, and projects are records: typed, versioned, editable through
the dashboard, and read at dispatch time by `execution`. Registry owns the
shape of those records and their invariants; it does not own how they are
persisted (that is the persistence adapter) or how they are used at runtime
(that is `execution`).

## RoleName

Identifier for a role. Lowercase + hyphens, unique within the registry.

## SubstrateClass

Where an agent runs: Podman, Docker, LXC, SSH, or VM. Picked per role. The
spawner implementation must match (today only the Podman path exists).

## PackageManager

Which package manager the spawner invokes to install a role's declared
`binaries` at spawn time. Apk (Alpine) or Apt (Debian/Ubuntu). Chosen per
role; no heuristic from the image name.

## McpServer

An MCP server declaration on a role. Names a symbolic server id and either
an image or a local binary. Consumed by the spawner at spawn time (not yet
runtime-enforced).

## Mount

A host→container bind mount on a role. Optionally read-only. Used by
Claude-Max roles to bring the `claude` binary and credentials into the
container without baking them into an image.

## WorkspaceMount

A role's declaration that it wants the brief's workspace bind-mounted into
its container. Names only the container-side path (e.g. `/workspace`) and
a read-only flag; the host path is chosen by the daemon at brief dispatch
from the allocated `BriefWorkspace`. A role without a `WorkspaceMount`
runs with no brief-scoped scratch space (echo / naughty / speaker etc.);
a coder-style role with one can clone, edit, and commit a working tree
that later roles in the same brief also see.

## AgentRole

The full specification of one kind of agent container: name, version, model
hint, system prompt, base image, substrate class, package manager, inline
entrypoint script, extra binaries to install, MCP servers, baseline tool
allowlist, baseline permit scope, env passthrough list, mounts, optional
workspace mount, and a `sccache` flag that wires the container to the
shared sccache-redis compile cache over the `agentry-net` podman network.

The role also carries `extra_bootstrap`: extra shell commands executed as
part of the container's bootstrap sequence, one per entry, appended AFTER
the package-manager install and BEFORE the role's entrypoint script.
Typical use: `rustup component add rustfmt clippy` for rust-based roles.
Empty means no extras. The role may also carry `exitpoint_script`: an
optional bash program the spawner runs AFTER the entrypoint exits 0,
BEFORE the terminal verdict. Used for role-local gates that augment the
entrypoint's work (e.g. a coder running `quality-hygiene --fix` before
emitting `shipped`). `None` means the entrypoint is solely responsible
for the terminal event.

Roles may pair up as sibling reviewers. The `agentry-self-host-v0` team
has two reviewers as siblings in `message_graph` (both list the coder as
their rework-target upstream via `MessageEdge`): `reviewer-mechanical-agentry`
(cargo fmt/clippy/test — machine truth) and `reviewer-claude-agentry` (LLM
review — naming, design, clarity, invariants). The current scheduler
executes them sequentially (mechanical first, then claude); the DAG
scheduler in issue #13 will enable parallel execution. A Blocker from
either reviewer rewinds to the coder, bounded by `team.max_retries`.

## TeamName

Identifier for a team topology. Lowercase + hyphens, unique within the
registry.

## MessageEdge

A directed routing edge in a team's message graph: from one role to another,
optionally naming a payload key whose contents become `PermitOverrides` on
the downstream role's permit.

## PermitOverrides

The intersection operator applied to a downstream role's freshly-minted
permit. Narrows `tool_allowlist` and `fs:read:*` / `fs:write:*` scope; empty
fields are no-ops. Emitted by an upstream role via a `Message` event and
extracted by the daemon along a `MessageEdge` that declares the carrier key.

## TeamTopology

The full specification of one team: name, version, role list, message graph,
terminal role, retry budget. Fetched by the daemon when a brief names this
team as its topology.

## ProjectSlug

Identifier for a project. Lowercase + hyphens, unique.

## StandingOrders

A project's durable policy: narrative context, default budget shape, default
escalation mode, and any other knobs the team's roles may consult at
dispatch time.

## Project

The full project record: slug, display name, standing orders, creation
timestamp. Briefs may optionally name a project; when they do, the team's
roles receive the project's standing orders as part of their startup bundle.

A project may optionally carry `repo_url` + `base_branch`. When both are
set, briefs dispatched under this project get their workspace allocated as
a git worktree off a shared bare clone of `repo_url`, tracking
`base_branch`. Briefs without a project fall back to reading `target_repo`
+ `base_branch` from `brief.payload` — this is the transitional path until
every brief carries a project.
