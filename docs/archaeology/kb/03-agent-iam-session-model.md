# Agent IAM + Session Model: AEGIS Work Permits + Episodic Memory

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-08-agency.md` (D-007, AEGIS section)
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-13-memory.md` (session handoff)

**1-line summary:** Zero-trust work permits (ed25519 signed JSON) grant executables/env/paths/network; coupled with episodic session memory that handoffs state between agent invocations.

---

## AEGIS Work Permits (C-001, Future IAM)

**Current status:** 34 unit tests, ZERO consumers. Intentionally deferred (security phase).

**Design:** Cryptographically signed JSON work permits that grant:
- `executables` — which binaries the agent can run
- `env_vars` — environment variables the agent can access
- `paths` — filesystem paths the agent can read/write
- `network` — allowed network egress (DNS domains, IPs)

**Model:** ed25519 signature verification at agent startup. Orchestrator mints permit, agent reads + validates signature. Revocation via TTL + re-issue.

**Why future?** The orchestrator currently relies on gatekeeper (stopgap). Real fix: enforce via AEGIS permits at container entry point. No need for in-band permission checks.

---

## Session Handoff Pattern (RFC-13)

Agents are ephemeral (spawn, run, die). But knowledge must persist. Three memory tiers:

| Tier | TTL | Role | Per-Agent |
|------|-----|------|-----------|
| LongTerm | 365 days | Strategic decisions, architectural lessons | A0 explicit writes only |
| ShortTerm | 24h | Sprint lessons, task decisions, decisions_made | lead-dev writes (agent polls) |
| Episodic | 30 days | Session state, handoff, portfolio snapshot | `SessionHandoffPort.read_handoff(session_id)` |

**SessionHandoffState captures at session end:**
- `session_id`, `ended_at`
- `next_session_brief` — verbatim brief for next agent invocation
- `portfolio_state` — what was accomplished

**Implementation:** `RedisHandoffStore` with tags `["session", "handoff"]`, 30d TTL.

**Next-session injection:** Orchestrator reads handoff, injects into next agent's system prompt. Agent continues from where previous session left off.

---

## Coupling Model

1. **Agent spawn → AEGIS permit validation** (signature check)
2. **Agent start → session memory injection** (handoff state + episodic tier + relevant ShortTerm entries)
3. **Agent end → SessionHandoffPort.write_handoff()** (portfolio state + next_session_brief)
4. **Orchestrator respawn decision** (based on health, budget, condenser signals) → loop to step 1

---

## Why Interesting for v2

AEGIS sidesteps the "How do we trust containers?" question—trust is cryptographic, not behavioral. Session handoff solves the "stateless container" problem without warm state: agents are fully ephemeral but knowledge persists, so they never start from zero.

