# Orchestrator Ops: Self-Management Domain (RFC-19)

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-19-orchestrator-ops.md`

**1-line summary:** The orchestrator manages agents but doesn't manage itself (16 binaries, no deployment manifest, no graceful shutdown, no upgrade strategy). RFC-19 defines self-health monitoring, deployment topology, graceful shutdown protocol.

---

## The Problem

Currently: 16 binaries across 4 LXCs with NO:
- Deployment manifest (which binary runs where)
- Upgrade strategy (how to update without dropping in-flight tasks)
- Self-health monitoring (what happens when daemon crashes)
- Graceful shutdown (drain tasks before restart)
- Configuration management (runtime config without recompile)

PHOSPHENE monitors agents. Nobody monitors the orchestrator.

---

## Proposed Solution

### 3 New Crates (RFC-19)

| Crate | Role |
|-------|------|
| `orchestrator-ops-domain` | Health model, shutdown protocol, upgrade state machine |
| `orchestrator-ops-contracts` | Port traits: `HealthProbePort`, `ShutdownPort`, `ConfigPort` |
| `orchestrator-ops-adapters` | Systemd integration, health endpoints, config sources |

### Deployment Topology (Currently Inferred)

```
| LXC | Binaries | Role |
|-----|----------|------|
| claude-lxc (148) | A0 (Claude Code session) | Architect + methodology anchor |
| coder-a1 (177) | coder-daemon, coder-mcp-server, git-mcp-server | Coder agent |
| coder-a2 (178) | coder-daemon, coder-mcp-server, git-mcp-server | Coder agent |
| coder-a3 (179) | coder-daemon, coder-mcp-server, git-mcp-server | Coder agent |
| lead-dev (180) | lead-dev-daemon, events-daemon, guidance-daemon, signal-daemon, context-monitor | Lead-dev agent |
| OVH (remote) | ship-gateway, webhook-daemon | Delivery |
```

**This table is INFERRED, not authoritative.** RFC-19 creates a manifest.

---

## Daemon Health Model

```
Starting → Healthy → Degraded → Unhealthy → Dead
             │                      │
             └──────────────────────┘ (recovery)
```

**Health probes:** Each daemon publishes heartbeats to Redis:
- Key: `orchestrator:health:{daemon_name}:{instance_id}`
- TTL: 2× heartbeat interval
- Payload: timestamp, tasks_in_flight, error_count, memory_usage

---

## Graceful Shutdown Protocol

```
SIGTERM received
    ↓
Stop accepting new tasks
    ↓
Drain in-flight tasks (configurable timeout, default 60s)
    ├── All tasks completed → Clean exit
    └── Timeout → Persist task state to Redis → Exit
                   (tasks re-queued on restart via Scheduler)
```

**Key requirement:** No task is lost. Either complete it or persist state for re-queueing.

---

## Rolling Restart (Upgrade Strategy)

1. Drain daemon instance A (graceful shutdown)
2. Deploy new binary to instance A
3. Start instance A (catches up from Redis streams)
4. Repeat for B, C...

**Prerequisites:**
- All daemons must be stateless (state in Redis, not in-memory)
- All stream consumers must track position (consumer groups)
- New binary must be backward-compatible with in-flight task format

---

## Status

**What exists:** Agent health monitoring (RFC-08/14). Recovery decisions (RespawnDecider).

**What's new:** Daemon health probes, graceful shutdown handler, rolling upgrade strategy, deployment manifest, configuration management.

---

## Why Interesting for v2

This solves the "production reliability" problem without k8s. Simple health probes + Redis streams + graceful shutdown = resilient system. The stateless-agent + Redis-state pattern makes restarts cheap and safe. No data loss, no warm state maintenance.

