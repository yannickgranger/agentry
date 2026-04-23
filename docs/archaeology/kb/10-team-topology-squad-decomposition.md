# Team Topology: Squad Decomposition (Incomplete but Designed)

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-07-execution.md` (SquadState section)
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-13-memory.md` (active_issues, squad_states)

**1-line summary:** For large tasks, coders can decompose into squad (multiple agents per task, each handling a module). SquadState tracks module status. NOT YET PERSISTENT.

---

## Squad Path (vs Single Coder Path)

From RFC-07 (Execution Domain):

**Single Coder path:**
```
TaskAssigned → SetupPhase → UnderstandPhase → DiscoverPhase →
DesignReviewPending → DesignReviewApproved →
ImplementPhase → IntegrationPhase → RealityCheckPhase → CodeReady
```

**Squad path (decomposed):**
```
DecompositionDecided → Modules[
  Pending → InProgress → Complete | GateBlocked
] → SquadIntegrationGate → AllModulesCompatible → CodeReady
```

---

## SquadState (In-Memory Only)

Currently not durable (BR-4 gap).

```rust
SquadState {
  module_list: Vec<Module>,
  status_per_module: HashMap<ModuleId, ModuleStatus>,
  file_scope: Vec<FilePath>,
  assigned_coder: CoderAgentId,
}
```

Each module has its own gate sequence. Squad completes when ALL modules pass.

---

## Module Assignment Strategy (Not Codified)

From agent-protocols.md (Lead-Dev Protocol):

> "The lead-dev sets gate requirements via `set_requirements` for all 11 gates, based on issue type."

Implied but NOT documented: How does lead-dev decide to decompose into squad? What are the decomposition criteria?

**Current practice (inferred):**
- Large features decompose into modules (domain + adapter pairs)
- Lead-dev assigns coders to modules manually
- Each coder sees their module only, not the full task

---

## Known Gaps

1. **BR-4 (HIGH):** SquadState is in-memory only — not durable across restarts
2. **Not codified:** Squad decomposition criteria, assignment algorithm, module dependency graph
3. **Missing:** Integration validation (do modules compose correctly?)
4. **Missing:** Squad escalation model (what if one module is blocked and another isn't?)

---

## Memory Tier Integration

From RFC-13, ShortTerm memory captures:
- `active_issues`
- `decisions_made`
- `squad_states` — what squad is executing, module status

This allows handoff between A0 sessions or human escalation: "Here's the current squad decomposition and status."

---

## Why Interesting for v2

Squad decomposition is a clever pattern: one task becomes multiple parallel streams. But it's unfinished—the system can describe squad state but can't make decomposition decisions, validate module compatibility, or persist squad state. This is low-hanging fruit for v2: finish the design, add persistence, and you unlock parallel task execution.

