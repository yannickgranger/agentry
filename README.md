# agentry

Minimal orchestrator for ephemeral agent containers. Methodology as data, enforcement as physics.

## What it does

Reads a `Brief` off a Redis stream. Resolves the named `TeamTopology` from a registry of role-DAGs. For each role in the team: mints + narrows + signs a `WorkPermit`, allocates a per-brief workspace if the role declares one, spawns a fresh container on a user-chosen substrate (Podman today), enforces the permit on every `tool_call` the agent emits, routes inter-role messages per the team's message graph, records a verdict.

Topologies range from full-pipeline (coder + reviewer-mechanical + reviewer-claude + shipper + ci-watcher) down to single-role (offline auditor). The planner role (`agentry-planner-v0`) decomposes a meta-brief into child briefs, picking each child's topology by task signature. The auditor role (`agentry-self-audit-v0`) inspects develop for unused dependencies via cargo-udeps and auto-dispatches fix briefs through the chain-trigger — no human in the loop.

The orchestrator doesn't know what "TDD", "gate", or "review" mean — that's what team topologies encode.

## Quickstart

```bash
# 1. Bring up local podman infra (idempotent).
just dev-redis-up        # agentry's dev Redis on 127.0.0.1:6380
just agentry-net-up      # bridge network + agentry-sccache-redis

# 2. Build + seed the registry.
cargo build --release --workspace
./target/release/orchestrator key-gen      # one-time ed25519 key
./target/release/orchestrator seed         # roles + teams → Redis

# 3. Run the daemon + dashboard.
./target/release/orchestratord &           # XREADs agentry:briefs, spawns containers

# 4. Submit a brief.
./target/release/orchestrator submit examples/verify-M0.json

# 5. Watch it.
redis-cli -p 6380 -a "$(cat ~/.config/agentry/redis.password)" --no-auth-warning \
    XREVRANGE agentry:verdicts + - COUNT 1
# or: http://localhost:7800 for the dashboard SSE view.
```

## Architectural shape

### Four data records

| Record | Redis key | Purpose |
|--------|-----------|---------|
| `Brief` | `agentry:briefs` (stream) | Unit of work: project, topology ref, payload, budget, escalation |
| `AgentRole` | `agentry:role:{name}:v{N}` | Container spec: image, package manager, inline entrypoint script, binaries, permit scope, mounts, optional `workspace_mount`, optional `sccache` |
| `TeamTopology` | `agentry:team:{name}:v{N}` | Role list + message graph + terminal role + retry budget |
| `Project` | `agentry:project:{slug}` | Slug, standing orders, budget, escalation default |

All four are edited via dashboard forms. No YAML files.

### Crates

- `orchestrator-types` — pure data + serde. Brief, AgentRole, TeamTopology, Project, WorkPermit, Event, Verdict, WorkspaceMount.
- `orchestrator-runtime` — daemon, Spawner trait + Podman adapter, permit broker, workspace lifecycle, inline-script bootstrap, CLI.
- `orchestrator-dashboard` — Axum + htmx + SSE + webhook intake.

### Topologies

Built-in topologies (in addition to internal probes):

- `agentry-self-host-v0` — full pipeline (coder → reviewer-mechanical + reviewer-claude → shipper → ci-watcher). Default for feature work.
- `agentry-bugfix-v0` — drops reviewer-claude. For sub-30-LOC mechanical fixes where CI suffices.
- `agentry-spec-edit-v0` — drops both reviewers. For specs/docs-only changes; merged-PR CI is the only gate.
- `agentry-discovery-v0` — single-role archaeologist that produces `discovery.json` from cfdb + graph-specs.
- `agentry-planner-v0` — archaeologist + planner; planner decomposes a meta-brief intent into child briefs and emits `next_brief_refs` for chain-trigger dispatch.
- `agentry-verify-v0` — single-role verifier; runs the meta-brief's `success_criteria` after children resolve. DOL composer combines child + verifier verdicts into the meta verdict.
- `agentry-self-audit-v0` — offline auditor. Runs cargo clippy/build/test/udeps against develop, persists findings as trace events, auto-dispatches `agentry-bugfix-v0` fix briefs for each unused-dep finding.

### Spawner behavior

Each role spawns on a stock public base image (`docker.io/library/alpine:3.21` or `docker.io/library/debian:bookworm-slim`). The role's inline `entrypoint_script` is delivered via the `AGENTRY_SCRIPT` env var; the spawner installs the declared `binaries` through `package_manager` (`apk` or `apt`) and execs the script. Every container joins the `agentry-net` podman network so it can reach `agentry-sccache-redis` by DNS name when `sccache=true`.

Enforcements baked into the spawner:

- **Permit broker:** every stdout `tool_call` event is audited and checked against the permit's `tool_allowlist` + `permit_scope`. Violations kill the container and emit `VerdictKind::PermitViolation`.
- **Wall-clock timeout:** when the brief's `budget.max_wall_seconds` is set, the stdout-read loop is wrapped in `tokio::time::timeout`. On elapse, `podman stop -t 1` runs and the verdict is `Failed` with reason `"wall-clock budget exceeded"`.
- **Workspace lifecycle:** if any role in the team declares `workspace_mount`, the daemon allocates `/var/mnt/workspaces/agentry-work/briefs/<brief_id>/`, bind-mounts it into each opting-in role, and tears it down only when the brief actually ships (or its diff is already pushed as a PR — `review-blocked*` verdicts). Every other failure mode (`failed: acceptance`, `failed: claude-timeout`, `failed: stalled`, `failed: spawner-error`, anything unrecognized) preserves the dir for forensics.

### Agent contract

Every container's stdout is parsed line-by-line as NDJSON `Event`s. Each is mirrored to `agentry:brief:{id}:trace`. A `Done` event terminates the role. Tool-call events flow to `agentry:brief:{id}:audit` regardless of whether the broker allowed them. This protocol is Published Language between `execution` and any containerised agent, regardless of language — it's a handful of JSON shapes, not a Rust trait.

### Architecture gate

Every PR runs `graph-specs check` (concept-level equivalence between `specs/concepts/*.md` and the pub surface of every crate) plus `cfdb extract` (a full fact-graph dump, archived). Tool revisions pinned in `.cfdb/cfdb.rev` + `.cfdb/graph-specs.rev`. Ban rules land one at a time in `.cfdb/queries/*.cypher`, each with its own justified PR and zero existing violations at introduction. Run the same checks locally with `scripts/arch-check.sh`.

## Workspace lifecycle

Per-brief workspaces live under `/var/mnt/workspaces/agentry-work/briefs/<brief_id>/` (override with `AGENTRY_WORKSPACE_ROOT`). Default teardown policy:

| Verdict | Disposition |
|---|---|
| `shipped` | TearDown |
| `review-blocked*` | TearDown (diff is in the forge as a PR) |
| `failed: acceptance` / `claude-timeout` / `stalled` / `spawner-error` | Preserve |
| any other / unknown | Preserve (default safe) |

Preserved workspaces accumulate disk — Rust `target/` is the dominant cost — so they need periodic GC. Use the `agentry-workspace` CLI for triage and lifecycle:

```bash
agentry-workspace list                       # table: brief_id, branch, age, disk_usage_mb, last_verdict
agentry-workspace path <brief_id>            # absolute host path; exit 1 if absent
agentry-workspace gc --older-than 7d         # remove preserved workspaces older than threshold
agentry-workspace gc --older-than 7d --dry-run   # list targets without removing
agentry-workspace remove <brief_id> --yes    # manual single-target removal
```

Recommended operator setup: a weekly `agentry-workspace gc --older-than 7d` cron entry. There is no auto-GC daemon — preservation defaults to safe, and reclamation is operator-driven.

## Infra prerequisites

Rootless podman on the dev box. Local `agentry-dev-redis` container on `127.0.0.1:6380` with a password at `~/.config/agentry/redis.password`. Local `agentry-sccache-redis` container attached to `agentry-net`. Ed25519 signing key at `~/.config/agentry/signing.key`. Never point agentry at a production Redis — the default config has pin tests to ensure `127.0.0.1` is the only baked-in target.

Claude-using roles (`coder-claude-agentry`, `reviewer-claude-agentry`, `archaeologist-claude-agentry`, `planner-claude-agentry`, plus the `claude-echo` probe) bind-mount a host directory at `/var/lib/agentry/transcripts/` into the container at `/transcripts/`. Each `claude -p` invocation tees its `--output-format stream-json --verbose` output into `/transcripts/${brief_id}<.role-suffix>.jsonl`, so on `timeout`/OOM/crash the partial trace survives container teardown for forensics. Create the host directory once before running:

```bash
sudo mkdir -p /var/lib/agentry/transcripts && sudo chmod 0755 /var/lib/agentry/transcripts
```

Operators on systemd setups should add `/var/lib/agentry/transcripts` to the orchestratord unit's `ReadWritePaths=` and rotate/GC the directory periodically — transcripts are ephemeral by design but accumulate one `.jsonl` per `claude -p` call.

## Further reading

- `CLAUDE.md` — project rules for Claude-driven development, including the cutoff.
- `docs/dogfood-protocol.md` — how to dispatch a brief into `agentry-self-host-v0` once the cutoff is live.
- `specs/concepts/*.md` — the DDD concept list checked by `graph-specs-rust`.
- `AGENTRY_RESUME.md` — current state for session resumption.
- `TODO.md` — tracker-style view of what's next.
- `docs/PROPOSAL.md` — original design proposal (2026-04-23).
_poc_v5: 2026-04-25_
