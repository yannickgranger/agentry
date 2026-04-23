# Escalation Hierarchy: 3 Toggle Modes (Autonomous / Supervised / Manual)

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-09-escalation.md`

**1-line summary:** Escalation routes through strict upstream hierarchy (Coder → Lead-dev → A0 → Human) with three configurable modes: autonomous (lead-dev handles coders only), supervised (A0 watches all), manual (A0 unsubscribed).

---

## State Machine

```
EscalationState: Raised → AssignedToHandler → HandlerResponded →
  Resolved (task unblocked) | ReEscalated (moved to next tier) | HumanRequired
```

---

## 3 Toggle Modes

From `kb-agent-zero/decisions/escalation-hierarchy-model.md`:

1. **Mode 1: Autonomous (Default)**
   - A0 subscribes to `escalation:leaddev` only
   - Lead-dev handles coder escalations autonomously
   - A0 intervention = explicit via override

2. **Mode 2: Supervised**
   - A0 subscribes to BOTH `escalation:coder` and `escalation:leaddev`
   - A0 can see all escalations, optionally intervene

3. **Mode 3: Manual**
   - A0 unsubscribes from all streams
   - Human operator handles escalations directly via devkit-server HTTP API

---

## Per-Tier Streams

| Tier | Stream | Handler | Status |
|------|--------|---------|--------|
| Coder level | `escalation:coder:{agent_id}` (S-033) | LeadDev | LIVE |
| LeadDev level | `escalation:leaddev` (S-034) | A0 | ORPHAN |
| A0 level | `escalation:a0` (S-035) | Human | ORPHAN |

**Note:** Naming mismatch gap (GAP-009): devkit-server uses `agency:coder:{taskId}:escalations` (S-040), orchestrator uses `escalation:coder:{agent_id}`. Needs alignment.

---

## Separation from PHOSPHENE

**Distinct:** Escalation = task guidance (work blockage). PHOSPHENE = process health (agent stuck/looping).

**Coupling:** PHOSPHENE alerts → ACL-7 (Observation → Escalation) → escalation stream → lead-dev inbox. But currently PHOSPHENE writes directly to `agent:lead-dev:inbox`, bypassing escalation domain (BR-11).

---

## Implementation

- **Orchestrator side:** escalation-contracts (C-043, 4 unit, 4 integ tests), escalation-adapters (C-042, 7 unit, 2 integ)
- **devkit-server side:** TaskEscalation Doctrine entity, EscalationController HTTP routes, DoctrineEscalationService (persists + publishes to Redis)
- **Missing:** ACL-7 wiring (code exists, not deployed)

---

## Why Interesting for v2

The three toggle modes let you shift operational responsibility without architectural change. Start autonomous (lead-dev owns it), escalate to supervised if needed, go full manual if the system is unstable. No code changes—just stream subscriptions.

