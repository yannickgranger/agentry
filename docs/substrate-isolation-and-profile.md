# Substrate isolation and per-repo profile

How a brief's container is assembled, and how the project being worked
on (the *target_repo*) declares its own competence rather than agentry
hardcoding it. Companion to `dogfood-protocol.md` — that doc explains
*how to dispatch briefs*; this one explains *what the brief lands in*.

## The split

agentry holds the generic primitives. Each target_repo declares the
project-specific bits.

| What | Where | Authoring |
| --- | --- | --- |
| Role definitions (substrate class, image, mounts, permits, entrypoint) | `seed/roles/<role>-v<N>.json` in `yg/agentry` | Engineering — versioned |
| Tool packs (binaries, allowed-tools, prompt fragments, bootstrap) | `seed/packs/<pack>.json` in `yg/agentry` | Engineering — versioned |
| Per-project overlay (which packs apply, default acceptance, gates) | `.agentry/profile.toml` in **each target_repo** | The project's owners |
| Operator state (host binaries, credentials, runtime seed dir) | `~/.local/bin/`, `~/.config/agentry/`, `~/.local/share/agentry/seed/` | Operator |

The role JSONs are project-agnostic. A coder-claude role does not know
or care what project it is operating on. The project tells agentry what
it needs by checking in `.agentry/profile.toml`. The daemon fetches
this file at brief dispatch via the forge contents API and augments the
role's hardcoded `tool_packs` with whatever the profile declares.

This replaces the older N-projects-equals-N-coder-roles anti-pattern.
One role, many consumers, project-owned config.

## The per-repo profile

`/.agentry/profile.toml` in the target_repo. Schema in
`specs/concepts/profile.md`. Canonical example in `yg/agentry`'s own
`.agentry/profile.toml`.

```toml
[coder]
tool_packs = ["quality-fast"]

[reviewer]
tool_packs = []

[acceptance]
default = "cargo run -p quality-fast --bin quality-mech --release --quiet && bash scripts/arch-check.sh"

[methodology]
gates = []
```

Sections are optional. A profile with only `[coder]` is valid.
Unknown keys error at parse time (`#[serde(deny_unknown_fields)]`).

The daemon's fetch path:

```
GET https://<forge>/api/v1/repos/<owner>/<repo>/contents/.agentry/profile.toml?ref=<base_branch>
```

- 200 + valid TOML → parsed, augmented onto roles
- 404 → no profile, role defaults stand
- Other errors → WARN, brief proceeds with role defaults

Augmentation is **additive**. A role declaring `tool_packs = ["X"]` and
a profile declaring `[coder] tool_packs = ["Y"]` produces effective
`["X", "Y"]` (deduplicated). A profile cannot strip a pack the role
declared. This preserves operator-set guarantees.

Augmentation is **per role kind**. `profile.coder` augments coder
roles; `profile.reviewer` augments reviewer roles. Other role kinds
(shipper, ci-watcher, ac-verifier) are not augmented.

`acceptance.default` fills in `payload.acceptance` when the brief
omits it. The brief author can still override per-brief.

`methodology.gates` lists skill names the methodology runner sequences
as pre-conditions before the coder accepts the brief. Empty list = no
gates.

## The container isolation envelope

Every brief role spawns into a podman container. The role JSON
declares the envelope: image, substrate class, package manager,
binaries, mounts, allowed_tools, permit_scope, passthru_env,
workspace_mount, sccache, extra_bootstrap.

```
Brief dispatched
       │
       ▼
Daemon fetches role + profile.toml
       │
       ▼
Spawner resolves tool_packs from Redis,
merges them into role's effective config
       │
       ▼
podman run --rm \
    --security-opt label=disable \
    --network=<role-policy> \
    -v <host-bin>:/usr/local/bin/<bin>:ro \
    -v <workspace>:/workspace \
    -v ~/.claude/.credentials.json:/root/.claude/.credentials.json:ro \
    -v ~/.config/agentry/claude-container-settings.json:/root/.claude/settings.json:ro \
    --env BRIEF_ID=... --env BRIEF_KIND=... --env BRIEF_BASE_BRANCH=... \
    --env GITEA_TOKEN=... \
    <image>
```

### Mounts

Five categories:

1. **Host binaries** — `claude`, `coder-claude-runner`,
   `reviewer-claude-runner`, `ship`, `ra-query`, `dead-pub-check`,
   `quality-fast`, `rtk`. Operator-installed at `~/.local/bin/<name>`
   and bind-mounted to `/usr/local/bin/<name>:ro`. The role JSON
   declares the mount; the operator is responsible for the host-side
   binary existing.

2. **Credentials** — `~/.claude/.credentials.json` for Claude Max OAuth.
   Read-only.

3. **Container settings** — `~/.config/agentry/claude-container-settings.json`
   maps to `/root/.claude/settings.json` and declares the in-container
   PreToolUse hook (`rtk hook claude` on every Bash call) plus the
   permission allowlist. Read-only.

4. **Transcripts** — `/var/lib/agentry/transcripts:/transcripts` (rw).
   The runner tee-writes every `claude -p` call here so trace events
   can be reconstructed offline. Critical mount: a permission failure
   here means tee silently drops and the agent exits with a bare 2.
   The spawner runs `preflight_transcripts_mount` before the role
   starts to surface this as a structured `Error::Config` instead.

5. **Workspace** — when the role declares `workspace_mount`, the
   daemon-allocated `BriefWorkspace` (see `specs/concepts/execution.md`)
   bind-mounts at the role's declared `container_path` (typically
   `/workspace`). Read-write for the coder; read-only for reviewers.
   When the workspace is a git worktree, the spawner additionally
   bind-mounts the `.clones/` root at its host path so the worktree
   gitdir pointer resolves inside the container.

### SELinux

Rootless podman on Fedora/Silverblue cannot read host-owned files
through SELinux unless either the source is relabeled (`:z`/`:Z`) or
the container's MAC is disabled (`--security-opt label=disable`).
Relabeling the source mutates the host filesystem, which is worse for
shared host directories like `~/.local/bin/`. The spawner takes the
disable path: any role with mounts gets `--security-opt label=disable`
added unconditionally. This is acceptable because the mounts are all
operator-controlled paths and the container is otherwise unprivileged.

### Network

The role's `permit_scope` declares allowed outbound destinations
(`net:allow:api.anthropic.com`, `net:allow:agency.lab`, etc). The
spawner translates these into the container network policy. There is
no implicit "allow all" — a role can only reach hosts it explicitly
permits. Sccache adds `net:allow:<sccache-host>` automatically when
the `[sccache]` config section is set.

### Operator-installed host binaries

The spawner soft-fails when a role's declared `~/.local/bin/<tool>`
mount source is missing on the host: it logs a structured WARN
(`<tool> host binary missing at <path>; coder gate will be skipped —
run 'just <tool>-binary' on the host`) and skips the mount. The
in-container runner falls back to `command -v <tool>` and emits a
trace event reporting the gate-skip. The brief still runs, just
without the corresponding gate.

Two exceptions where this fail-soft is **not** desirable: `claude`
itself and the `claude-container-settings.json` hook target. If
`claude` is missing the container has no LLM and the role exits 127
immediately. If the settings file references a binary the container
can't resolve (the rtk hook on Bash), every Bash tool call inside
claude returns 127 and the agent fails fast with no useful output.
The operator is expected to keep host binaries declared in role mounts
present.

## How a brief lands

1. Operator (today: a Claude session acting as captain) authors a
   brief payload — verbs only, no free-form. See `dogfood-protocol.md`
   for the payload schema and the verb vocabulary.

2. Brief is XADDed to the daemon's submission stream, signed with
   `signing.key`.

3. Daemon validates the signature, resolves the topology (today:
   `agentry-self-host-v0`), allocates a `BriefWorkspace`, and fetches
   `.agentry/profile.toml` from the target_repo.

4. For each role in the topology DAG, the spawner:
   - resolves the role's `RoleRef` against the registry
   - resolves declared `tool_packs` from Redis (`agentry:tool_pack:<name>:<version>`)
   - merges packs into the role's effective config (`role::merge_role_with_packs`)
   - augments with the profile's per-kind overlay (`profile.coder` or
     `profile.reviewer`)
   - constructs the podman invocation
   - streams stdout/stderr to the trace ring; tees claude transcripts
     to `/var/lib/agentry/transcripts`
   - records the role's terminal verdict on the brief's state stream

5. On the team's terminal verdict (`Shipped` or terminal `Failed`),
   the lifecycle FSM driver tears the workspace down (or retains it
   for audit, per `TerminationDisposition` in
   `specs/concepts/execution.md`).

## Where to fix what

| If it broke … | Edit … | Need a brief? |
| --- | --- | --- |
| A host binary isn't installed at `~/.local/bin/<tool>` | The host (install / symlink) | No — operator action |
| `~/.config/agentry/agentry.toml`, signing key, or container-settings JSON | The host config dir | No — operator action |
| A role's mounts, allowed_tools, permits, image, or bootstrap | `seed/roles/<role>-v<N>.json` in `yg/agentry` | Yes — dispatch into `agentry-self-host-v0` |
| A tool pack's binaries, allowed-tools, or prompt fragment | `seed/packs/<pack>.json` in `yg/agentry` | Yes — dispatch |
| A target_repo's tool packs, acceptance, or gates | `.agentry/profile.toml` in **that target_repo** | Yes — dispatch into `agentry-self-host-v0` with `target_repo: <that-repo>` |

Operator-side fixes do not require a brief and do not push commits.
Repo-side fixes go through `agentry-self-host-v0`. The cutoff in
project CLAUDE.md applies: no Claude-authored direct push to
`yg/agentry` after issue #8 closed.

The runtime seed location (`~/.local/share/agentry/seed/`) is
operator-managed and may diverge from the repo. When it does, repo and
runtime should be reconciled via brief — the canonical source is
always the repo.

## Verifying the loop

A healthcheck brief — trivial scope, full pipeline:

```bash
# Construct a minimal payload that touches one file and passes existing
# acceptance. Sign + XADD. Watch the trace stream.
ORCHESTRATORD_HEALTHCHECK=1 cargo run -p orchestrator-cli -- \
    submit --brief examples/healthcheck-brief.json
```

Then:

```bash
redis-cli -h 127.0.0.1 -p 6380 -a "$(cat ~/.config/agentry/redis.password)" \
    XREVRANGE agentry:trace + - COUNT 50 \
    | grep -E 'brief=brf_healthcheck|verdict|spawning'
```

A green run produces: brief received → workspace allocated → coder
spawned → coder Shipped → reviewer-mechanical Shipped → reviewer-claude
Shipped → shipper Shipped → ci-watcher Shipped → workspace cleaned.

A red run that exits 125 at the container layer means the substrate is
broken (missing host binary, SELinux misconfiguration, podman daemon
not running). A red run that completes Authoring but exits with a
useful claude error means the role is broken (bad allowlist, bad
permits, missing pack). The split tells you whether the fix is
operator-side or repo-side.
