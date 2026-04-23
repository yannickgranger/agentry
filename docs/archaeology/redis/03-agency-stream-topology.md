# Agency — Redis Stream Topology & Agent Lifecycle

**Source keys:**
- `architecture:agency-flow`
- `architecture:ecosystem`
- `architecture:stream-topology`
- `architecture:protocols`
- `architecture:sprint-infrastructure`
- `architecture:mcp-per-role`
- `architecture:status-protocol`
- `architecture:streaming-gate`
- `architecture:implementation-status`
- `architecture:defense-in-depth`
- `architecture:observability`
- `deployment:guidance-daemon`
- `deployment:lead-dev`
- `deployment:phosphene`
- `contract:agency-bus-extension`
- `contract:agency-terminal-stdin-daemon`
- `infra:ip-remap-2026-02-28`
- `audit:v7:phase3:*` (10 keys)

## Why interesting for v2
User said: "Agents = REAL containers / REAL processes / REAL messaging. Substrate user-chosen: LXC, Docker, VM, Alpine+ssh. Dev = podman. EPHEMERAL — spawn → work → trash → respawn. No warm state."

v1 already designed and partly deployed exactly this topology. The stream names, ephemeral coder pattern, guidance-daemon stdin-injection, and mood FSM are all genuine gems. v2 should KEEP these patterns and DROP the code.

## Verbatim — The stream topology (the good parts)

### Pipeline streams
- `agency:tasks` — Dashboard → PO (raw tasks)
- `agency:tasks:assigned` — PO → Orchestrator
- `agency:tasks:dlq` — dead-letter queue (every critical stream has a `:dlq`)

### Coder management (the pattern worth stealing)
- `agency:coder:requests` — Lead-dev → CoderRouter (task assignment)
- `agency:coder:{id}:logs` — guidance-daemon pipes stdout → Redis
- `agency:coder:{id}:status` — parsed `##STATUS` blocks
- `agency:coder:{id}:guidance` — reverse channel: pokes/approvals injected as prompts
- `agent:{coder-id}:inbox` — direct address

### Control plane
- `agency:commands` — Dashboard → orchestrator
- `agency:command-results` — orchestrator → Dashboard
- `agent:lead-dev:inbox` — PHOSPHENE/orchestrator alerts
- `agency:escalations` — Lead-dev → A0

### Events
- `agent:events` — all agents → EventsHandler (lifecycle)
- `agency:signals` — stream signal extraction
- `agency:checkpoint:approved` / `:rejected` — validator → commit-pipeline

## ##STATUS protocol — extract-without-modifying-Claude-CLI pattern
Coders emit structured markers in stdout. Guidance-daemon on the agent host captures stdout, XADDs raw logs to Redis. LogProcessor on orchestrator parses:

```
##STATUS phase=N task="..." progress="X/Y" blocker="..." next="..."
##CHECKPOINT Phase:N Summary:...
##ESCALATE Topic:... Context:... Question:...
##DONE
```

**Reverse channel (KEY PATTERN):** guidance-daemon subscribes to `agency:coder:{id}:guidance`, writes payload atomically to `/tmp/inject/{id}/guidance.txt`, a `UserPromptSubmit` hook on the coder reads + clears the file + injects as `additionalContext` to the next Claude turn. File cleared after read.

This is how you "inject guidance into a running LLM session without modifying the CLI." Load-bearing for v2.

## MCP-per-role — the gated-tools insight
From `architecture:mcp-per-role` (verbatim):
> CODERS (LXC 301-303):
> - devkit ONLY: checkpoint(), status(), diff()
> - NO gitea (prevents tunneling to push)
> - NO git tools (checkpoint model)
> - Key insight: "If coder sees git_push → LLM tunnels toward it, skips quality phases"

User's v2 directive: "Tools = CONFIGURABLE per agent-type / group. Orchestrator installs; agent performs. All tool use MONITORED."
This is exactly the "tool tunneling" prevention pattern — if the tool isn't installed, the LLM can't reach for it.

## Gitless agents (RESOLVED from audit v7)
From `audit:v7:phase3:gatekeeper`:
> Gatekeeper (ship-authorize) solves the wrong problem. The real fix is making agents GITLESS — no git in their deployment at all. If agents can't push, you don't need a setuid binary to prevent unauthorized pushes.

Terminal finding. v2 should design agents gitless from day zero.

## PHOSPHENE Mood FSM (observability/alerting)
```
CALM → VIGILANT (5min no status) → ANXIOUS (errors) → PANIC (15min post-poke) → SHUTDOWN
  ├─ VIGILANT/ANXIOUS: emit PokeMessage to guidance stream (auto-retry)
  ├─ PANIC: emit Alert to agent:lead-dev:inbox (human-escalate)
  └─ SHUTDOWN: cascade brake, terminate session
```
5-layer detection: heartbeat, pheromone, status stream, Bayesian fusion, SIS publisher.

Worth keeping conceptually — a "liveness/hang detector with auto-escalation" is architecturally right even if the Bayesian fusion is overkill for MVP.

## Dead-end: 5-layer "Defense in Depth"
From `architecture:defense-in-depth` — user had elaborate "5 layers" (Clarity Gate, Phase 4 Checkpoint, ##STATUS Heartbeat, MCP Signal Tools, PHOSPHENE). This layering is overthinking. v2 core loop is simpler: "issue → discover → prescribe → code → gates → ship." Don't rebuild 5 layers.

## Dead-end: 62-crate monolith
From `project:orchestrator-v2:shape`:
> 62 crates in a monorepo with cross-crate ACL translators
Explicitly listed as anti-pattern. Do not recreate provider-trait-per-crate topology.

## Contract-first signal passing (from contract:* keys)
Three streams for terminal ↔ stdin-daemon:
- `cmd:a0-agency` (command with correlation_id, prompt, priority, thread_id)
- `res:a0-agency` (response with is_final, metadata.source, correlation_id)
- `evt:a0-agency` (events: tool_call, thinking, context_high)

Routing rule: match correlation_id → thread. Match `metadata.source="LeadDev:{project}"` → create/find project thread. Default → human thread.

## IP layout (real infra, don't improvise)
See `infra:ip-remap-2026-02-28` — complete host plan for Proxmox LXCs:
- agency-redis (400), agency-orchestrator (403), agency-lead-dev (404), agency-coder-a1/a2/a3 (405-407)
- gitea on 101-103, rustfs on 105, meilisearch on 159

v2 doesn't need this exact plan but needs the discipline: pin IPs, document DNS, use Infisical for creds.
