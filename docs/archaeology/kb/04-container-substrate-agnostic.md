# Container Substrate Abstraction: LXC/Docker/SSH, Feature-Gated

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-08-agency.md` (D-006)
- `/var/mnt/workspaces/agency-orchestrator/site/src/reference/agent-infra.md`
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-19-orchestrator-ops.md`

**1-line summary:** Orchestrator supports LXC (default), Docker (feature-gated), SSH adapters via `agent-lifecycle` crate; infrastructure provider is pluggable via `InfraProvider` port.

---

## The Abstraction

From RFC-08 (D-006): "Docker adapters KEEP. Orchestrator supports multiple infra backends (LXC default, Docker option)."

**Crate structure:**
- `agent-lifecycle-contracts` (C-029) — defines `InfraProvider` port trait
- `agent-lifecycle-adapters` (C-028) — 46 unit, 6 integ tests. Feature-gated:
  - `#[cfg(feature = "docker")]` → DockerAdapter
  - `#[cfg(feature = "ssh")]` → SshExecutor
  - `#[cfg(feature = "lxc")]` → ProxmoxRuntime (default)

**Actual deployment (RFC-19):**
```
| LXC Name | Binaries | Role |
|----------|----------|------|
| claude-lxc (148) | A0 | Architect session |
| coder-a1..3 (177-179) | coder-daemon, git-mcp-server, coder-mcp-server | Coder agents |
| lead-dev (180) | lead-dev-daemon, events-daemon, guidance-daemon, signal-daemon | Lead-dev agent |
| OVH (remote) | ship-gateway, webhook-daemon | Delivery |
```

**Ephemeral spawning:** `agent-bootstrap` (external repo) clones from Proxmox templates (VMID 600-699), bootstraps via SSH:
1. UpdateHosts → ConfigureDns → InstallCaCert → WriteClaudeMd → CreateWorkspace → DeployStdinDaemon
2. `stdin-daemon` bridges Redis Streams ↔ Claude CLI (or Gemini via adapter)

---

## Key Design Decisions

1. **No k8s abstraction.** Direct container API (Proxmox REST, Docker socket, SSH).
2. **Feature gates, not runtime selection.** Choose substrate at compile-time; no "plugin architecture" overhead.
3. **Completely stateless agents.** No warm containers. Spawn → run → trash → respawn. State lives in Redis.
4. **Guidance injection via file:** Orchestrator writes to `/tmp/inject/{id}/guidance.txt` → Claude hook reads → published to logs stream.

---

## Gaps to Address

- **GAP-006:** agent-bootstrap depends on agent-lifecycle feature branch (not develop). Merge pending.
- **X-008:** Vendored copy of lifecycle crates in agency-orchestrator. Should be git dep or workspace member.
- **Missing:** No documented substrate selection matrix (which substrate for which workload).

---

## Why Interesting for v2

Feature-gated infrastructure is simpler than runtime plugin systems. You pick your substrate (LXC for dev, Docker for CI, SSH for OVH), compile once, deploy once. No dynamic discovery, no config hell. The abstraction is explicit (ports) but lightweight (adapters are <200 LOC each).

