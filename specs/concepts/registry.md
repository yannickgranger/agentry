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

Beyond `agentry-self-host-v0` (full pipeline), two lighter topologies exist. `agentry-bugfix-v0` drops `reviewer-claude-agentry` for sub-30-LOC bug fixes where mechanical CI is sufficient. `agentry-spec-edit-v0` drops both reviewers for specs/docs-only changes; the merged-PR CI run catches any spec/code mismatch.

The planner role picks each child's topology from the task signature: `agentry-spec-edit-v0` for specs/docs-only edits, `agentry-bugfix-v0` for sub-30-LOC Rust bug fixes, `agentry-self-host-v0` (default) for everything else. The meta-brief's `payload.child_topology` provides the fallback if the planner omits a child's topology.

The `auditor-claude-agentry` role and `agentry-self-audit-v0` topology emit cargo clippy/build/test reports as trace-stream events. Offline (no LLM, no forge, no claude mounts); reports persist in `agentry:brief:<id>:trace` for Phase 2 consumers.

Phase 2 — auditor runs `cargo +nightly udeps --output json`, emits one child brief per unused normal/dev/build dep targeting `agentry-bugfix-v0`. Dispatch via `emit_message "_chain_trigger" {next_brief_refs}`.

Roles using `BASH_PRELUDE` export `GIT_SSL_NO_VERIFY=true` and `CARGO_NET_GIT_FETCH_WITH_CLI=true` so cargo can fetch private git deps from internal forges (agency.lab, git.lab) whose certs aren't in the container CA bundle. This matches the pattern projects' own CI workflows already use.
_poc_v4: 2026-04-25_
