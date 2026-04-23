# Methodology as Physics: Constraint Inversion Pattern

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/site/src/reference/architectural-decisions.md` (AD-7)
- `/var/mnt/workspaces/agency-orchestrator/site/src/reference/process-evolution.md`

**1-line summary:** Don't say "follow these rules"—make the system state-based so methodology becomes the only path forward.

---

## The Inversion

**Traditional approach:** "Coders must follow the 8-phase protocol. Enforce with policy/training."

**Physics approach:** "You are in state RED_FAILED. To leave this state, you must write SUPER_GREEN code. There is no other exit. The system does not accept alternatives."

From AD-7:

> "Methodology is not constraints to negotiate — it is the physics of the environment. Three enforcement layers:
> 1. Framing (prompt wording)
> 2. Judges as security gates (validation layer)
> 3. State machine makes judges unavoidable (structural)"

**The proof:** Early sprints showed ~30% rework rate with policy-based process. By Jan 7, 2026 (coder-gate POC), agents followed methodology perfectly. Key difference: **gates became the only way to declare "done."** No workaround. No escalation path around gates.

---

## Framing Techniques (Proven)

From RFC-07, the methodology self-enforcement pattern uses five framing techniques:

1. **Violation Frame** — calling out methodology violations explicitly
2. **Interface Wall** — contracts are inviolable boundaries  
3. **State Distinction** — RED/GREEN/SUPERGREEN are distinct, named states
4. **SuperGreen Reward** — clean code earns SuperGreen immediately
5. **Constraint Inversion** — constraints enable (discovery gate clarifies scope), not restrict

---

## Current Implementation

- **Pipeline:** All 8 phases codified. Checkpoint (phase 4) feeds into gate pipeline.
- **Judges:** 4-tier validation (recipes → evidence → Haiku → specialized judges)
- **State persistence:** `GateSequenceState` in RedisSCRManager (canonical)
- **Proof:** Dec 2025 baseline (30% rework) → Mar 2026 (near 0%, gate-enforced)

---

## Why Interesting for v2

This flips the agent autonomy problem. You don't need to trust agents to follow process—you make the system so agents can't leave until process is satisfied. Cheaper than monitoring, more reliable than instructions.

