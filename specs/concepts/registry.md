# Registry

The bounded context that owns the *catalog of what teams and agents exist*.
Roles, teams, and projects are records: typed, versioned, editable through
the dashboard, and read at dispatch time by `execution`. Registry owns the
shape of those records and their invariants; it does not own how they are
persisted (that is the persistence adapter) or how they are used at runtime
(that is `execution`).

## RoleName

Identifier for a role. Lowercase + hyphens, unique within the registry.

## RoleRef

A version-pinned reference to a role: `(name, version)`. Team topologies
hold `RoleRef` values rather than bare names so a topology committed today
keeps resolving to the exact role specs it was authored against — even as
new role versions are registered.

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

## TemplateContext

Runtime-config-derived values made available to JSON role files at seed
time. The seed context derives `home`, `forge_net_allow` (from
`ForgeConfig.default_host`), `forge_write_permits` (one per
`ForgeConfig.allowed_owners` entry), and an optional `sccache_net_allow`
(from `SccacheConfig.endpoint`), then passes the bundle through to the
role-directory loader. The loader expands `~/`, `${HOME}`,
`${FORGE_NET_ALLOW}`, and `${SCCACHE_NET_ALLOW}` substrings in any string
field eligible for templating, and treats `${FORGE_WRITE_PERMITS}` as a
list-spread token in `permit_scope`: a `permit_scope` element exactly
equal to that token is replaced inline by every entry of
`forge_write_permits`. A sole `${SCCACHE_NET_ALLOW}` element with no
configured sccache endpoint is dropped rather than emitted as the empty
string. JSON role files therefore stay declarative without baking forge
host, allowed owners, or sccache endpoint into source.

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
team as its topology. `MessageEdge` and `TeamTopology` both carry
`#[serde(deny_unknown_fields)]`, so any unknown key in a serialized topology
is rejected at parse time — vocabulary integrity is enforced structurally,
not by a runtime check.

## TeamValidationViolation

A single violation surfaced by the team-topology validator. Names the
offending `path` (e.g. `roles[2]`, `message_graph[0].from`, `terminal_role`),
a `ViolationKind` classifying it, and a human-readable `detail`. The
validator collects violations across all five checks without short-circuit,
so a single call may return multiple kinds in one batch.

## ViolationKind

Classification of a `TeamValidationViolation`. `Type` covers empty / zero
where a non-empty / non-zero value is required. `Reference` covers a name
that does not resolve in its target set (a role missing from the registry,
an edge endpoint missing from `topology.roles`, a terminal role missing
from `topology.roles`). `Topological` covers absence of any entry role,
unreachability of `terminal_role` from any entry, or roles in `roles[]`
that no edge references. `Acyclic` covers a cycle in `message_graph`.
`MultipleTerminals` covers more than one role with no outbound edges, or
a unique sink that is not the declared `terminal_role`.

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

The `ac-verifier-claude-agentry` role is an instance of `AgentRole` slotted between the coder and the reviewer pair in `agentry-self-host-v0`. It reads `brief.payload.acceptance_criteria` (a `Vec<String>`) plus the coder's git diff against `base_branch`, asks claude for a strict-JSON per-AC verdict, and emits one `Finding(blocker, category=ac-violation)` per failed AC plus `done rework_needed`. Empty/missing AC list, missing binary, or invalid claude JSON all degrade to `done shipped` — `reviewer-claude-agentry` is the architectural backstop. The team's `message_graph` uses a dual-inbound trick: the existing `coder→reviewer` edges are preserved, and new `coder→ac-verifier` and `ac-verifier→reviewer` edges are appended AFTER them. The daemon's `team.incoming(reviewer).first()` rework lookup therefore rewinds to the coder (the corrective upstream), not to the (non-corrective) ac-verifier.

The `ac-verifier-grok-agentry` role is the Grok-backed sibling of the claude variant in the AC verifier role family (brief 4 of #134). Same bash shape (read bundle, short-circuit on empty AC list, fetch base_branch + diff HEAD, build binary stdin JSON, pre-flight `command -v ac-verifier-grok`, degrade to `done shipped` on any failure), but the bind-mounted binary is `ac-verifier-grok` and it talks to the xAI API via `XAI_API_KEY` instead of the host `claude` CLI. Permit scope grants `net:allow:api.x.ai`; `XAI_API_KEY` is in `passthru_env`. Registered in seed but NOT yet wired into a team — brief 5 enables parallel mode (the claude variant alone runs in `agentry-self-host-v0` until then).

A project may also carry `max_concurrent_briefs: Option<u32>`. When set, this caps the number of briefs from this project that the dispatcher will run concurrently; falls back to the global `Config.max_concurrent_briefs` (default 4) when `None`. The cap is enforced at dispatch time via a per-project semaphore; FIFO ordering of the brief stream is preserved. Briefs without a project share a global `_global` pool.

## AcVerifierProvider

Trait implemented by every LLM backend the ac-verifier binary can call. Single method `verify(system, user) -> io::Result<String>` returns the provider's raw response; the verifier core parses the JSON. Brief 2 ships `ClaudeProvider` only; briefs 3 (Gemini) and 4 (Grok) add per-file siblings as text-only adds. Tests use `MockProvider` to drive the core logic without spawning a real LLM.

## ClaudeProvider

The `claude -p --output-format text` provider impl. Shells out to the host claude CLI (bind-mounted at /usr/local/bin/claude inside the ac-verifier container) with the concatenated `system\n\n---\n\nuser` prompt as a single positional arg. No timeout in the binary — the role's bash script wraps the whole invocation in `timeout $CLAUDE_P_TIMEOUT`.

## GeminiProvider

The Gemini `generateContent` provider impl. Shells out to `curl` and POSTs to `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent` with the system + user prompt split into `system_instruction` and `contents`, `responseMimeType=application/json`. Reads `GEMINI_API_KEY` from the process env and passes it to the curl child via env (never as a literal Rust-level argv entry). Default model is `gemini-3-flash-preview`. No timeout in the binary — the role's bash script wraps the whole invocation in `timeout $CLAUDE_P_TIMEOUT`.

## GrokProvider

The xAI Grok provider impl, sibling of `ClaudeProvider`. Shells out to `curl` to POST to `https://api.x.ai/v1/chat/completions` with `Authorization: Bearer $XAI_API_KEY`. The API key is read from the binary's process env (`XAI_API_KEY`) and passed to curl via env + stdin so it never appears in argv. Default model is `grok-4-fast` (matches the existing grok-echo role); request body uses `max_tokens: 4096` to give the verifier room for the verdicts JSON. Powers the `ac-verifier-grok-agentry` role (registered in seed but not yet wired into a team — brief 5 enables parallel mode).

## Input

The JSON shape the ac-verifier binary reads on stdin: `acceptance_criteria` (`Option<Vec<String>>`), `diff` (raw unified-diff text), and `verb_body` (the brief's verb body). Built by the role's bash script from the startup bundle's `brief.payload` + a fresh `git diff origin/<base_branch>..HEAD`.

## Outcome

The terminal value of an ac-verifier run. Two variants: `Shipped` (all ACs met, AC list empty/missing, or graceful degradation on provider/parse error) and `Rework { findings: Vec<Finding> }` (one or more failed ACs). The bash script maps `Shipped → emit_done "shipped"` and `Rework → emit_finding_model + emit_done "rework_needed"`.

## Finding

A single failed-AC record produced by the ac-verifier core. Fields: `severity` (always "blocker"), `category` (always "ac-violation"), `message` (AC text + claude's evidence). Distinct from `review::ReviewFinding` because the ac-verifier crate intentionally avoids depending on `orchestrator-types`; the bash script adapts each `Finding` to the structured `Finding` event that the daemon ingests via `emit_finding_model`.

The planner role picks each child's topology from the task signature: `agentry-spec-edit-v0` for specs/docs-only edits, `agentry-bugfix-v0` for sub-30-LOC Rust bug fixes, `agentry-self-host-v0` (default) for everything else. The meta-brief's `payload.child_topology` provides the fallback if the planner omits a child's topology.

The `auditor-claude-agentry` role and `agentry-self-audit-v0` topology emit cargo clippy/build/test/udeps/ra-query reports as trace-stream events. The role bind-mounts host-built ra-query at /usr/local/bin/ra-query (operator runs `just ra-query-binary` to provide it). Two ra-query stages run in addition to udeps: `unwraps --severity critical` (auto-dispatches fix-child briefs for top-K=3 files by critical_count) and `complexity --threshold 15` (report-only). Reports persist in `agentry:brief:<id>:trace`.

Phase 2 — auditor runs `cargo +nightly udeps --output json`, emits one child brief per unused normal/dev/build dep targeting `agentry-bugfix-v0`. Dispatch via `emit_message "_chain_trigger" {next_brief_refs}`.

Roles using `BASH_PRELUDE` export `GIT_SSL_NO_VERIFY=true` and `CARGO_NET_GIT_FETCH_WITH_CLI=true` so cargo can fetch private git deps from internal forges (agency.lab, git.lab) whose certs aren't in the container CA bundle. This matches the pattern projects' own CI workflows already use.

Roles using `BASH_PRELUDE` derive their `claude -p` timeout from the `CLAUDE_P_TIMEOUT` env (default 1200s). Spawner can override per-role for tighter budgets (e.g. reviewer-claude: 300s; archaeologist: 600s) without touching role scripts.

The `ac-verifier-gemini-agentry` role is a Gemini-provider variant of the AC-verifier role family, sibling to `ac-verifier-claude-agentry`. It mirrors the claude variant's shape (read brief's `acceptance_criteria` + coder's diff, ask the model for a strict-JSON per-AC verdict, emit one blocker `Finding` per failed AC, degrade to `done shipped` on missing binary / missing key / parse error). Its bind-mount is `~/.local/bin/ac-verifier-gemini` only — Gemini doesn't need the host claude CLI or credentials. `passthru_env` carries `GEMINI_API_KEY`; `permit_scope` allows `generativelanguage.googleapis.com`. The role is registered in the seed but is NOT yet wired into any team — brief 5 introduces parallel-pipeline mode that fans `agentry-self-host-v0` out to all three providers.

Commandant-authored workflows now cohabit with engine-seeded topologies in the same Redis catalog. `seed.rs` writes its hardcoded `TeamTopology` literals via the overwriting `save_team`; the `orchestrator team register` CLI writes Commandant-authored topologies via the atomic `register_team_strict` (first-writer-wins). Both target the same `agentry:team:<name>:v<version>` key namespace, so the daemon's `fetch_team` is agnostic about which path produced the entry. See `runner.md` for the full operator-facing surface.
_poc_v4: 2026-04-27_
