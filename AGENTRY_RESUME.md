# AGENTRY — Session-Portable Resume Plan

**Canonical recovery document.** Any future Claude session can read this file + `git log` + Redis state + the `TODO.md` next-to-it and continue exactly where the last session stopped.

---

## What this is

`agentry` is a minimal orchestrator for ephemeral agent containers. It is the v2 replacement for `agency-orchestrator` (declared dead; graveyard). The design was crystallized 2026-04-23 via a four-miner archaeology pass over the KB, Redis memory, 28 local workspaces, and the v1 graveyard. Full proposal: `docs/PROPOSAL.md`.

**One-line summary:**
A bus driver for ephemeral, capability-minimized agent containers talking via typed events, with methodology externalized to Redis-backed typed records and observability forced by design.

## North star

**M9 green:** a real qbot-core issue is closed end-to-end by agentry. PR merged, zero human keystrokes between brief submission and merge.

## The four data records (Redis, typed, dashboard-edited)

| Record | Key pattern | Purpose |
|--------|-------------|---------|
| `Brief` | `agentry:brief:{id}` | Unit of work; carries project ref + topology + payload + budget + escalation |
| `AgentRole` | `agentry:role:{name}:{version}` | Container spec: model, system prompt, tool allowlist, substrate class, binaries, MCPs |
| `TeamTopology` | `agentry:team:{name}:{version}` | Roles + message graph + permit-override rules |
| `Project` | `agentry:project:{slug}` | Name, forges, budget cap, escalation default, default topology |

## The three crates

| Crate | Purpose | Target LOC |
|-------|---------|-----------|
| `orchestrator-types` | Pure types + serde (Brief, AgentRole, TeamTopology, Project, Verdict, WorkPermit, Event) | ~250 |
| `orchestrator-runtime` | Daemon: Redis consumer, Spawner trait + podman adapter, permit broker, CLI | ~900 |
| `orchestrator-dashboard` | Axum + htmx + SSE + webhook endpoint | ~400 |

**Total new LOC at M9: ~1,550.**

## Agent I/O contract (the one required interface)

An agent is any process that:
1. Reads Brief + Permit + RoleConfig as one JSON document on stdin.
2. Emits events as NDJSON on stdout: `{"type":"event","at":"<iso>","payload":{...}}`.
3. Reads inbox messages (from teammates) as NDJSON on fd 3 (bound to a Redis stream).
4. Writes outbox messages as NDJSON on fd 4 (bound to a Redis stream).
5. Emits `{"type":"done","verdict":"shipped|failed|escalated","at":"<iso>"}` as last event, then exits.

Claude CLI, Grok CLI, Gemini CLI, Python script, shell script — anything that satisfies this contract is an agent.

## Redis namespace & endpoints

- **Redis: LOCAL podman container** — `127.0.0.1:6380` (inside `agentry-dev-redis` container, port 6380 to avoid any collision). Password at `~/.config/agentry/redis.password` (0600, gitignored). `just dev-redis-up` brings it online idempotently.
- **NEVER use prod Redis LXCs** for agentry dev. Both LXC 401 (`.152` — A0 session/memory) and LXC 522 (`.189` — PROD-AGENCY streams) are off-limits for this project's dev traffic.
- All agentry keys live under `agentry:` prefix (clean-separated from v1 `agency:` graveyard)
- Key streams:
  - `agentry:briefs` — brief submission inbox (XADD/XREAD)
  - `agentry:verdicts` — verdict log (append-only)
  - `agentry:brief:{id}:trace` — per-brief event trace
  - `agentry:agent:{id}:inbox` / `outbox` — per-agent messaging
- Gitea: `https://agency.lab:3000/` (token in KeePassXC under `gitea/agency.lab`)
- Podman: local on dev box, user-mode, default substrate
- Dashboard: `http://localhost:7800` when `just dev-up` is running

## Recycle inventory (gems salvaged from existing workspaces)

| Source | Role in agentry | How to reuse |
|--------|-----------------|-------------|
| `~/workspaces/agency-orchestrator/crates/agency-aegis` | WorkPermit type + signer + audit | Copy into `orchestrator-runtime` under `permit/` module (≈2k LOC, drop-in) |
| `~/workspaces/agency-orchestrator/crates/agent-events` | Canonical event vocabulary | Copy types into `orchestrator-types` |
| `~/workspaces/agency-orchestrator/crates/forge-subprocess` | Shipper role tool | Install binary in shipper container only |
| `~/workspaces/stdin-daemon` | Agent-in-container wrapper template | Pattern for agent roles; don't depend on the crate |
| `~/workspaces/agent-lifecycle` | Spawner trait reference | Base the agentry Spawner trait shape on it; Podman adapter is new |
| `~/workspaces/mcp-forge`, `mcp-rules`, `mcp-signal`, `mcp-devkit` | Drop-in MCP servers | Install in relevant roles' containers |
| `~/workspaces/cfdb`, `~/workspaces/graph-specs-rust` | External services roles call (not embedded) | Spec-guardian + archaeologist roles invoke via MCP |

## Roadmap (M0 → M9)

Deploy pattern for every milestone: `just dev-up` (systemd user units on dev box).
Verify: every issue ships with a brief in `examples/verify-M<N>.json` that produces a verdict on `agentry:verdicts`.

| # | Ship | Verify | Recycles | LOC |
|---|------|--------|----------|-----|
| M0 | Runtime + podman spawner + echo role + CLI | `orchestrator submit examples/verify-M0.json` → verdict=shipped + event "hello" | podman | ~400 |
| M1 | Dashboard (Axum + htmx + SSE) | Replay M0; localhost:7800 shows spawn → event → teardown in real time | — | ~250 |
| M2 | Typed registry editor (forms for Role / Team / Project) | Create role via form → submit brief → spawns correctly | — | ~200 |
| M3 | Permit broker (AEGIS signing + tool-call enforcement) | Role with `allowlist:[read]` attempts `write` → killed; violation on dashboard | agency-aegis | ~200 |
| M4 | Two-role message routing via agency-bus | `speaker → listener` team → trace has both events in order | agency-bus | ~100 |
| M5 | Real Claude CLI role inside container (stdin-daemon pattern) | `reader` role reads /workspace/README.md via MCP; dashboard shows content bytes | stdin-daemon | ~200 |
| M6 | Upstream `permit_overrides` narrow downstream scope | Synthesizer declares `files_to_touch:[src/a.rs]`; coder attempts `src/b.rs` → blocked | — | ~80 |
| M7 | E2E on toy repo with shipper role | Team ships a trivial PR to a dev forge repo; `gh pr view` confirms; dashboard shows full trace | forge-subprocess, mcp-forge | ~200 |
| M8 | Project + cron + webhook triggers | Linux cron fires `orchestrator submit steward.json`; steward brief opens a PR editing ROADMAP.md on qbot-core | — | ~120 |
| M9 | First real qbot-core issue closed | A trivially-specced open issue is picked, worked, PR merged on qbot-core | cfdb, graph-specs (external) | ~0 |

## Drift-prevention rules (re-read at every milestone start)

1. **No verdict, no close.** Every milestone's commit body links the `agentry:verdicts` entry + the dashboard trace.
2. **LOC ceiling per milestone: ~250.** Over = scope-creep. Revisit.
3. **No new primitives between M0–M9.** Four records + one event contract. Anything beyond = M10+.
4. **Dashboard-first.** No feature without a visible surface (M1+).
5. **Dogfood ratchet from M7 on.** Every M<N+1> PR must be opened by the v2 orchestrator itself.
6. **Every milestone has a kill switch.** `orchestrator abort --all` works.
7. **Replay principle.** Brief + verdict + trace always on Redis; any run replayable from dashboard.
8. **No edits outside `/var/mnt/workspaces/agentry/`.**
9. **No push to any repo other than `agency:yg/agentry`.**
10. **No InMemory in production paths.** Test doubles feature-gated only.

## Explicitly deferred to M10+ (do NOT build these before M9)

- Additional substrates beyond podman (LXC, Docker, SSH, VM).
- PHOSPHENE-style absence telemetry.
- Budget/escalation UI beyond a numeric cap.
- Multi-project concurrency controls.
- Episodic memory / cross-brief knowledge transfer.
- Auto-retry strategies.
- Visual team-graph editor (forms are enough).
- Any "plugin architecture".

## Resumption protocol (for a fresh session)

Any future Claude session picking this up MUST:

1. `cd /var/mnt/workspaces/agentry && git status && git log --oneline -20`
2. Read `AGENTRY_RESUME.md` (this file) + `TODO.md` (next action) + `docs/PROPOSAL.md` (the full shape).
3. `mcp__memory__get key="project:agentry:resume"` — supplementary Redis state (last milestone completed, current focus, open TODOs).
4. `just verdicts` (or `redis-cli -h 127.0.0.1 -p 6380 -a "$(cat ~/.config/agentry/redis.password)" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 10`) — verify last verdicts.
5. `just dev-up` if starting fresh work session; verify dev infra is up.
6. Continue from "Next concrete action" in `TODO.md`.

**DO NOT** invent new primitives, add a new crate, or re-open deferred features. If tempted, stop and re-read drift rules above.

## Known limitations (M0 / M1 / M2 / M3 / M4 / M5a / M5b)

- **SELinux + bind mounts:** rootless podman on Fedora/Silverblue requires `--security-opt label=disable` to read host-owned files. Spawner auto-adds it whenever `role.mounts` is non-empty. Don't use `:z`/`:Z` — they relabel the host path.
- **Claude Max via bind-mounted binary + `~/.claude/.credentials.json`.** `settings.json` is also mounted read-only. Container's `HOME` is `/root`; `~/.claude` maps from the host.

## OLD Known limitations (M0 / M1 / M2 / M3 / M4 / M5a)

- **Single-daemon only.** `orchestratord` uses `XREAD BLOCK $` — no consumer groups. Two daemons will both consume a brief → double-processing. Production uses a single systemd user unit; this is fine until M4+. Dodge: `pkill -f orchestratord` before `orchestratord`.
- **Permit signing is active from M3.** Run `orchestrator key-gen` once; key lives at `~/.config/agentry/signing.key` (0600). Without the key, `orchestratord` refuses to start.
- **Message routing is sequential only.** M4 ships role-A → role-B hand-off via accumulated `team_context.messages` in the next role's startup bundle. Roles run one-at-a-time; parallel team execution is M10+.
- **Registry editor is create-only.** M2 ships list + new + POST for Role/Team/Project. Edit (PUT/PATCH) + delete land when the need surfaces; until then version-bump on every save is the history trail.
- **No CSRF / auth on the dashboard.** Single-user LAN dev tool. Shipping to any shared network is a separate M10+ concern.
- **303 redirect + curl quirk:** curl by default does NOT downgrade POST → GET on a 303 (contrary to RFC 7231). Every form POST from curl must NOT use `-L`; browser clients follow the PRG pattern correctly.

## Frozen rules (do not re-open)

### Terminal rules
- **Claude is Claude Max subscription only — NEVER the per-token Anthropic API.** Any Claude agent subprocesses the host's `claude` CLI. No `ANTHROPIC_API_KEY`, no `anthropic` SDK, no HTTP to api.anthropic.com. Grok and Gemini APIs are fine (cheap/fast models). See `~/.claude/projects/-var-mnt-workspaces-agency-orchestrator/memory/feedback_claude_max_only.md`.

### Design points
- Front-door = any client (app/CLI/IDE) writing to `agentry:briefs`. Not an agentry agent.
- Dashboard = Axum + htmx + Alpine + Tailwind CDN. No SPA. Leptos islands reserved for a future drag-drop graph editor.
- Registry storage = Redis + typed dashboard forms. No YAML files.
- cfdb + graph-specs = role-embodied enforcers, not runtime-embedded.
- Phases = graph convention (synthesizer emits `permit_overrides` consumed downstream). Not a separate type.
- Triggers = Linux cron + one webhook route + `next_brief` on verdicts. No scheduler crate.
- Project = 40-line record; standing orders live there.

---

*Last updated: see `git log -1 AGENTRY_RESUME.md`. If this document drifts from the code, the code is right.*
