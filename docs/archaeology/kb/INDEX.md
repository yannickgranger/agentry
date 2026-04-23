# Archaeology Findings: Top 10 Novel Ideas for Agent-Zero v2

**Mining date:** 2026-04-23  
**Sources:** agency-orchestrator RFCs + site docs + phosphene repo  
**Scope:** Concepts, patterns, design decisions, NOT code critique

---

## Ranked by Novelty & Applicability to v2

### 1. **Tool Gating Enforcement** (01-tool-gating-enforcement.md)
MCP tools become conditional based on methodology state. The `write_impl` tool is unavailable until spec gate passes. Methodology enforcement moves from policy to physics.

### 2. **Methodology as Physics** (02-methodology-as-physics.md)
Don't say "follow these rules"—make the system so agents can't leave until process is satisfied. Constraint inversion: the environment is the teacher. Proven 30% → 0% rework.

### 3. **Agent IAM + Session Model** (03-agent-iam-session-model.md)
AEGIS work permits (cryptographically signed JSON) + episodic session handoff. Agents are fully ephemeral but knowledge persists. Zero-trust security without behavioral monitoring.

### 4. **Container Substrate Abstraction** (04-container-substrate-agnostic.md)
Feature-gated infrastructure (LXC default, Docker optional, SSH fallback). Pick substrate at compile-time, no runtime discovery. Direct APIs (Proxmox, Docker, SSH), no k8s layer.

### 5. **Front Door: Clarity Gate** (05-front-door-clarity-gate.md)
Cheap LLM validation (Grok, ~$0.01/task) + rule-based preflight before expensive pipeline. Reject malformed briefs early. First filter before intake.

### 6. **PHOSPHENE: 5-Layer Monitoring** (06-phosphene-observation-system.md)
Absence telemetry (agent broke its own expectations, not timeout). Mood FSM (Calm → Vigilant → Anxious → Panic → Shutdown). Classification system detects 7 behavior patterns.

### 7. **Escalation Hierarchy: 3 Toggles** (07-escalation-hierarchy-3-toggle.md)
Autonomous (lead-dev handles), Supervised (A0 watches), Manual (human direct). No code changes—just stream subscriptions. Operational dial.

### 8. **Super Green TDD (2-Step)** (08-super-green-tdd-not-3-step.md)
Reject RED → GREEN → REFACTOR. Use RED → SUPER GREEN (clean code first time). Forces discipline. Contract-first ensures correctness early.

### 9. **Orchestrator Self-Management** (09-orchestrator-ops-self-management.md)
RFC-19: The orchestrator manages agents but not itself. Proposes: deployment manifest, health probes, graceful shutdown, rolling upgrades. No k8s.

### 10. **Team Topology: Squad Decomposition** (10-team-topology-squad-decomposition.md)
Large tasks decompose into modules assigned to multiple coders. SquadState tracks module status. Incomplete (BR-4: not persistent). Low-hanging fruit for v2.

---

## Cross-Cutting Patterns

| Pattern | Files | Impact |
|---------|-------|--------|
| **Ephemeral + Persistent State** | 3, 4, 10 | Agents spawn/die/respawn; knowledge lives in Redis |
| **Physical Enforcement > Policy** | 1, 2, 8 | Constraints via tech, not rules |
| **Feature Gates > Runtime Plugins** | 4, 5 | Simplicity: compile once, deploy once |
| **Absence Telemetry** | 6 | Monitor expectations, not timeouts |
| **3-Toggle Operations** | 7 | Shift responsibility without code change |

---

## Gaps v2 Must Solve (Beyond KB)

1. **Squad persistence:** SquadState durable storage + decomposition algorithm
2. **AEGIS wiring:** Work permits into container entry point
3. **Front Door completion:** Sonnet Router frontend + context enrichment
4. **Escalation coupling:** ACL-7 (Observation → Escalation) wired
5. **Orchestrator manifest:** Explicit deployment topology (currently inferred)
6. **Tool gating redis:** Wire Redis-backed ToolGateSCRPort (currently InMemory stubs)
7. **Agent-zero KB structure:** Current KB is empty; v2 will populate it with findings

---

## What KB Does NOT Cover

- **Agent prompt engineering:** How to frame tasks to model (design, framing techniques exist in devkit-server, but no v2 KB equivalent)
- **Cost/benefit tradeoffs:** When to use Haiku vs Opus vs Gemini (decision made in AD-8, but no detailed cost matrix)
- **Dependency deadlock resolution:** What if module A in squad depends on module B, both blocked?
- **Tool allowlist per agent-type:** Which tools for coders vs lead-dev vs A0 (implicit in mcp-devkit, not codified)
- **Brief intake linguistics:** How to parse human intent into TaskPrompt (clarity_gate does semantic validation, but no intake grammar)
- **Redis topology SLA:** Which stream guarantees, what happens at high throughput (deployed but not specified)

---

**See individual files for detailed sources and "why interesting for v2" notes.**
