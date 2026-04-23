# Orchestrator v2 — A Minimal, Dogfoodable, Methodology-Free Agent Runtime

**Author:** archaeology-synthesis (Claude) · **Date:** 2026-04-23
**Status:** Proposal for user review. Not committed. Not code. A shape.

---

## TL;DR (3 lines)

The orchestrator is a **thin runtime** that spawns ephemeral agent containers from typed briefs, scoped to capability-minimized toolsets via signed work permits, communicating over Redis Streams, and monitored centrally. **Methodology is data** (YAML team topologies), **enforcement is physics** (missing tools = impossible violations), **observability is mandatory** (every tool call emits an event; a dashboard renders reality). Everything else lives outside.

---

## The ONE idea v2 is built on

> **Methodology is a team, not a runtime.**

v1 tried to teach Rust the meaning of "discover", "prescribe", "gate", "ship". 30,556 lines of enforcement code. 24% of the codebase. Commit cadence went 924/month → 96/month. The orchestrator ate itself.

v2 does not know what "TDD" means. It does not know what "gate" means. It knows three nouns: **Brief**, **AgentRole**, **TeamTopology**.

- Want TDD methodology? Write a TeamTopology with roles `tester → coder → reviewer`, message graph `tester must emit RED before coder starts`, permit for `coder` excluding the `push` tool. That **is** the methodology.
- Want a different methodology? Write a different YAML file.
- Want a new gate? It's a role with a tool allowlist that gates what subsequent roles can do.

**The orchestrator is a bus driver, not an English teacher.**

---

## What v2 is NOT (pins against my own bias)

- **Not Kubernetes.** Not a container scheduler. Not a service mesh. If you want k3s, you use k3s.
- **Not an LLM framework.** No LangChain/CrewAI shape. No prompt templates. No "memory" abstraction. Agents are CLI processes.
- **Not a methodology engine.** No gates, skills, judges, verdict-types, or phases baked in runtime.
- **Not Claude-centric.** An agent is anything that reads a brief from stdin and emits events on Redis.
- **Not a product.** One user (you). No multi-tenant. No SaaS. No plugin marketplace.
- **Not a rewrite of v1.** It's a different thing. v1's graveyard is a gem mine; we extract, not resuscitate.

---

## The three primitives

```
┌─────────────────────────────────────────────────────────────────┐
│  Brief  ──────▶  TeamTopology  ──────▶  [AgentRole × N]         │
│  (payload)       (pipeline)             (container spec)         │
└─────────────────────────────────────────────────────────────────┘
```

### Brief — the unit of work

An immutable JSON record:

```json
{
  "id": "brf_0193...",
  "submitted_by": "front-door-agent-0x12",
  "submitted_at": "2026-04-23T10:00:00Z",
  "topology": "qbot-issue-team",
  "payload": { "issue_ref": "qbot:yg/qbot-core#4300", "prompt": "..." },
  "budget": { "max_tokens": 500000, "max_wall_seconds": 3600, "max_usd": 5.00 },
  "escalation": "supervised",
  "parent_brief": null
}
```

Briefs never mutate. Scope change = new brief with `parent_brief` set. The brief log is append-only and replayable.

### AgentRole — the container specification

A YAML file in a git-tracked registry:

```yaml
# roles/coder-rust.yaml
name: coder-rust
model: claude-opus-4-7
system_prompt: "@file://prompts/coder-rust.md"
substrate_class: podman         # dev default; per-brief overridable
tool_allowlist:
  - read
  - edit
  - grep
  - bash:cargo
  - bash:rustfmt
mcp_servers:
  - ra-query
binaries:
  - rustc-1.91
  - sccache
permit_scope:
  - fs:read:/workspace/**
  - fs:write:/workspace/**
  - net:deny:*         # no github, no npm, no crates.io push
  - git:local-only     # physics: no git remote = no push possible
```

**Physics:** `coder-rust` cannot push because `git push` is denied at the permit layer, git-remote access is absent at the container layer, and the container is destroyed at task end.

### TeamTopology — the methodology

A YAML file in the same registry:

```yaml
# teams/qbot-issue-team.yaml
name: qbot-issue-team
roles:
  - archaeologist   # reads repo, produces facts (read-only)
  - prescriber      # reads facts, produces REUSE/CREATE decisions (read-only)
  - coder-rust      # implements, runs tests (no push)
  - reviewer        # reads diff, runs full quality checks (no write)
  - shipper         # ONLY role with git push; tiny allowlist

message_graph:
  - archaeologist -> prescriber
  - prescriber -> coder-rust
  - coder-rust -> reviewer
  - reviewer -> shipper
  - reviewer -> coder-rust          # loopback on reject
  - shipper -> _terminal

escalation_mode: supervised
retry_policy: { max_retries: 3, backoff: exponential }
terminal_role: shipper
```

**The methodology emerges from**: (a) which roles exist, (b) what tools they have, (c) what messages are allowed to flow between them. No enforcement code.

---

## The agent I/O contract (the ONE required interface)

An agent is a process that:

1. Reads its **brief + permit + role config** from stdin (JSON, once).
2. Emits **events** on its stdout in NDJSON: `{type, payload, at}`. Events are mirrored to Redis `agent:{id}:events` by the container runner.
3. Reads **inbox messages** from Redis `agent:{id}:inbox` (teammates' outputs).
4. Writes **outbox messages** to Redis `agent:{id}:outbox` (forwarded by orchestrator per topology).
5. Emits `{type: "done", verdict: "shipped|failed|escalated"}` as its last event.

That's the whole contract. Claude Code fits (via stdin-daemon wrapper). Grok CLI fits. Gemini CLI fits. A Python script fits. A shell script fits.

**Tool calls are events**, captured by the container runner and routed through the permit broker. The agent never touches Redis directly for permit decisions — the runner intercepts.

---

## Runtime loop (the whole orchestrator in one page)

```
1. Front-door agent puts Brief on stream `agency:briefs`.

2. Orchestrator XREAD → parse Brief → lookup TeamTopology.

3. For each AgentRole in topology:
     a. Mint signed WorkPermit (ed25519, AEGIS).
     b. Ask Spawner for the right substrate adapter (podman/docker/lxc/ssh/vm).
     c. Adapter creates a fresh rootless container with the role's
        binaries + MCP servers installed (RecipeExecutor from agent-lifecycle).
     d. Start container; inject brief+permit+role-config on stdin.
     e. Bind Redis inbox/outbox streams.

4. Orchestrator routes messages per topology.message_graph.
   - Does not inspect message content. Dumb router.
   - Every routed message is also mirrored to `agency:brief:{id}:trace`.

5. Permit broker subscribes to every agent's events stream.
   - On each tool call event: verify (agent_id × tool) ∈ permit.tool_allowlist.
   - On violation: kill container, record "permit_violation" verdict.
   - On budget exhaustion: kill container, record "budget_exceeded".

6. Terminal role emits `{type: "done", verdict: "shipped"}`.
   - Orchestrator tears down all containers (Spawner.destroy_all).
   - Appends brief → trace → verdict to durable log.
   - Front-door agent sees the verdict and replies to the human.
```

**The orchestrator does not**: plan, reason, judge, summarize, prescribe, gate, ship, merge, or compile. All of those are things *roles* do.

---

## Substrate model — user picks

```rust
pub trait Spawner: Send + Sync {
    async fn spawn(&self, role: &AgentRole, permit: &WorkPermit) -> Result<AgentHandle>;
    async fn destroy(&self, handle: AgentHandle) -> Result<()>;
    async fn health(&self, handle: &AgentHandle) -> Result<Health>;
}
```

Feature-gated implementations (all already exist in `agent-lifecycle`):

| Substrate | Feature | Target | Reused from |
|-----------|---------|--------|-------------|
| Podman    | `podman` (default) | Dev | New adapter, ~200 LOC |
| Docker    | `docker` | Dev/CI | `agent-lifecycle/docker` |
| LXC       | `lxc` | Prod lab | `agent-lifecycle/lxc` |
| SSH       | `ssh` | Any Linux box | `agent-lifecycle/ssh` |
| libvirt VM | `vm` | Prod isolated | New adapter, ~300 LOC |

Per-role `substrate_class` override. No k8s abstraction. No pre-baked agent-type images — every spawn is *fresh image + binary install via RecipeExecutor*.

---

## IAM & Permits — physics first, paper second

Three layers, in order of enforcement strength:

1. **Container layer (physics):** the binary / MCP / tool literally isn't present. `git push` in a role without git = unreachable.
2. **Permit broker layer (policy):** AEGIS-signed permit scopes tool calls. Tool call outside scope → container killed.
3. **Audit layer (accountability):** every tool call written to the brief's trace stream. Tamper-evident via permit signature.

Permit data model (recycled from `agency-aegis`):

```
WorkPermit {
  permit_id, agent_id, role, brief_id,
  tool_allowlist, permit_scope,
  budget { tokens, usd, wall_seconds },
  issued_at, expires_at,
  signature (ed25519 over all the above)
}
```

**The permit broker is 200 LOC of Rust wrapping `agency-bus`** — it's the only new IAM code v2 needs.

---

## Observability — dashboard-first, absence-telemetry-second

Every event lands on Redis. The dashboard subscribes via SSE and renders:

- **Briefs in flight** — one row per active brief.
- **Teams** — for each brief, one sub-row per role, live tool-call rate.
- **Tool calls** — scrolling log of `(agent, tool, args, result, permit_status, at)`.
- **Permit violations** — red banner.
- **Budget burn** — per brief, per role.
- **Verdict history** — immutable.

Day-1 dashboard = **one Axum + htmx binary, ~500 LOC**, reading from Redis. No React, no Leptos, no build tooling beyond `cargo`.

PHOSPHENE-style absence telemetry (monitor expectations, not timeouts) is a v1 gem we port in v2.1, not day 1.

---

## Recycling inventory — motto: avoid creating code

### Verbatim salvage from v1 graveyard

| From v1 | LOC | Role in v2 |
|---------|-----|------------|
| `agency-aegis` | 2,019 | WorkPermit type + signer + audit. Drop-in. |
| `quality-recipes` | 920 | Template for YAML-schema pattern (not the code — the *idea*: rules as data). |
| `workflow-engine` | 4,416 | Reference for how to model the runtime state machine cleanly (already passed unaided audit). |
| `agent-events` | 1,401 | Canonical event vocabulary. Drop-in. |
| `forge-subprocess` | 2,565 | Shipper-role tool for `gh`/`tea`. Drop-in. |
| `agency-llm-client` | 3,104 | Provider-neutral LLM caller (for cheap agents that don't shell out to Claude Code). |

### Verbatim reuse from external workspaces

| From | Role in v2 |
|------|------------|
| `stdin-daemon` | Agent-in-container wrapper template (one file, ~400 LOC pattern). |
| `agency-bus` | Typed Redis Streams transport. The message bus. |
| `agent-lifecycle` | 3-crate workspace with Docker/SSH/LXC adapters. Feeds Spawner directly. |
| `mcp-forge` | Reference MCP server for forge ops. Shipper role binary. |
| `mcp-rules` / `mcp-signal` / `mcp-devkit` | Drop-in MCP servers for relevant roles. |
| `cfdb` | External service; anti-drift x-ray. Roles can call it but it's not embedded. |
| `graph-specs-rust` | External service; spec gate. Same — service, not embedded. |

### New code

| Crate | Purpose | LOC estimate |
|-------|---------|-------------|
| `orchestrator-types` | Brief, AgentRole, TeamTopology, Verdict | ~300 |
| `orchestrator-registry` | YAML loader + validator for role/team files | ~200 |
| `orchestrator-runtime` | The daemon: Redis consumer, Spawner invocation, message router | ~500 |
| `orchestrator-permit-broker` | Tool-call monitor, permit enforcement | ~250 |
| `orchestrator-dashboard` | Axum + htmx, SSE, Redis subscriber | ~500 |
| `orchestrator-cli` | `submit`, `monitor`, `list-roles`, `list-teams`, `abort` | ~150 |

**Total new code: ~1,900 LOC.**
**Total v2 size (new + salvaged): ~17,000 LOC.**
**vs. v1: 127,000 LOC. v2 is 13%.**

---

## Bounded contexts — 3, not 62

### BC-1 — Briefing & Registry
Nouns: Brief, AgentRole, TeamTopology, Verdict.
Owns: intake stream consumption, YAML registry, verdict log.
Code: `orchestrator-types` + `orchestrator-registry`.

### BC-2 — Lifecycle & Capability
Nouns: Spawner, AgentHandle, WorkPermit, SubstrateAdapter.
Owns: container provisioning/teardown, binary & MCP install, permit minting, tool-call enforcement.
Code: `orchestrator-runtime` + `orchestrator-permit-broker` + recycled `agency-aegis` + `agent-lifecycle`.

### BC-3 — Visibility
Nouns: Event, Trace, Dashboard.
Owns: event bus, SSE, UI.
Code: `agency-bus` + `orchestrator-dashboard`.

Each context talks to the others through **ONE shared vocabulary** (`agent-events`). No ACL translators, ever.

---

## Day 0 / Day 1 / Day N dogfood briefs

### Day 0 — "Hello world"
Brief: `{ topology: "echo-team", payload: "echo hello" }`.
Team: one role `echo-agent` (reads brief, prints "hello" to stdout as event, emits `done`).
Proves: spawner works, brief flows, event stream flows, teardown works.

### Day 1 — "Ship the dashboard"
Brief: `{ topology: "qbot-issue-team", payload: "implement orchestrator-dashboard v0.1" }`.
Team: `archaeologist → prescriber → coder-rust → reviewer → shipper`.
Proves: real team, real permit enforcement, real PR shipped by v2 itself.

### Day 7 — "Fix a qbot-core issue end-to-end"
Brief: `{ topology: "qbot-issue-team", payload: "qbot:yg/qbot-core#4300" }`.
Proves: the orchestrator is actually useful. If this brief completes autonomously, **the project is worth continuing**. If it doesn't, we stop and reconsider.

If Day 7 is green, Brief 4 = "build me a landing page for X" with a `landing-team` topology. Brief 5 = "refactor trading strategy Z". The orchestrator is a factory.

---

## Evolution path — extensible without compile

| Change | Cost |
|--------|------|
| Add a new role | Write a YAML file. No compile. |
| Add a new team topology (= a new methodology) | Write a YAML file. No compile. |
| Add a new tool to a role | Add line to YAML. No compile. |
| Swap the model an agent uses | Edit YAML. No compile. |
| Add a new substrate (e.g. firecracker microVM) | Implement `Spawner` trait, add feature flag. One recompile. |
| Add a new primitive concept (e.g., Budget) | Rust change. One recompile. Rare. |

**You should need to recompile the orchestrator only when changing the 3 primitives or adding a substrate.** Everything else is config.

---

## Anti-patterns this proposal EXCLUDES (from v1 post-mortem)

1. No `{concept}-domain/-contracts/-adapters` triads. One concept = one crate with `mod port; mod adapter;`.
2. No InMemory doubles compiled into production binaries. Test-doubles feature-gated.
3. No RFC catalogue before code. Spec on demand; code is the proof.
4. No methodology in Rust. TeamTopology YAML or nothing.
5. No ACLs between contexts. ONE event vocabulary.
6. No horizontal `wave-0..wave-5` labels. Vertical slices only.
7. No Redis stream without a live consumer in the same PR.
8. No "delete or populate" issues. Unconsumed = deleted.
9. No setuid / gatekeeper binaries. Gitless agents (shipper is the only role with push).
10. No SHA-pin cron ceremonies. Binary does the ceremony.
11. No prose rules for split-brain. Tests or compile-time links.
12. No 20 RFCs before day 1. Each spec arrives with its code.

---

## Open questions where your call is load-bearing

### Q1 — Is the front-door agent part of v2 or external?
My lean: **v2 agent, homogeneous substrate**. Costs: orchestrator must support a "human channel" (Slack/Matrix/CLI) role. Benefits: no special-case code, front-door dogfoods itself. Alternative: external process that only writes to `agency:briefs`. Which do you prefer?

### Q2 — Dashboard stack
Lean: **Axum + htmx server-rendered**, zero build tooling beyond cargo. Grep-able, small blast radius. Alternative: Leptos + WASM (all-Rust but heavier stack). Your call.

### Q3 — Brief submission
Lean: **both** — CLI `orchestrator submit brief.yaml` writes to the Redis stream; scriptable. Anything else is a thin adapter on top.

### Q4 — What happens when a role fails repeatedly?
Lean: **retry policy is a TeamTopology field** (max_retries, backoff). If exhausted, terminal verdict = `failed`, front-door agent escalates to human via its channel. No built-in "recovery logic" in runtime. Agreed?

### Q5 — Registry format & storage
Lean: **YAML files in a git-tracked `orchestrator-registry/` repo**, loaded on orchestrator startup + SIGHUP reload. Human-reviewable PRs on methodology changes. Alternative: in-Redis (livelier but reviewless). Your call.

### Q6 — Cfdb + graph-specs integration
Lean: **they are external services, not embedded**. An `archaeologist` role calls cfdb; a `spec-guardian` role calls graph-specs. Neither is in the orchestrator core. Keeps the runtime methodology-free. Agreed?

---

## The first 10 commits (concrete, executable)

1. `orchestrator-types` crate with Brief/AgentRole/TeamTopology structs + JSON/YAML round-trip tests.
2. `orchestrator-registry` with `roles/echo-agent.yaml` and `teams/echo-team.yaml` + validator.
3. `Spawner` trait in `orchestrator-runtime` + `podman` adapter (uses `podman run` subprocess).
4. `RecipeExecutor` integration from `agent-lifecycle` — install a binary at spawn time.
5. Minimum viable echo-agent binary (reads brief on stdin, prints events to stdout, exits).
6. End-to-end Day 0 dogfood: `orchestrator submit echo.yaml` → container spawns → "hello" event → teardown.
7. `orchestrator-permit-broker` with AEGIS permit minting + tool-call verification.
8. `orchestrator-dashboard` — Axum + htmx + SSE → renders live brief.
9. Real roles: `archaeologist`, `prescriber`, `coder-rust`, `reviewer`, `shipper` YAML + matching Claude Code invocation wrappers (stdin-daemon pattern).
10. Day 1 dogfood: orchestrator ships its own dashboard (PR raised by `shipper` role on agency forge).

**Every commit is deployed to dev substrate (podman) and visible on dashboard. Nothing ships un-deployed.**

---

## One-line summary

> v2 is **a bus driver** for ephemeral, capability-minimized agent containers talking via typed events, with methodology externalized to YAML and observability forced by design.
