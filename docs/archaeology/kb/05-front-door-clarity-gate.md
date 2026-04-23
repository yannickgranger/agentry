# Front Door: Clarity Gate Pattern (Pre-Intake Validation)

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-01-front-door.md`
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-02-intake.md`

**1-line summary:** Before a task enters the pipeline, validate it via `ClarityGate` (LLM-based semantic validation) + `preflight-check` (rule-based evidence validation). Gate decision: PASS | BLOCK | WARN.

---

## The Gate

**Purpose:** Reject malformed briefs, fabricated references, unclear intent BEFORE they waste orchestrator time.

**Components:**
1. **ClarityGate** (`src/clarity_gate/` in orchestrator)
   - LLM-based validation using cheap models (Grok, ~$0.01/task)
   - Reads repository context via `repo_context.rs`
   - Returns `ClarityVerdict: Pass | Block | Warn`
   - Implementation: `CheapModelClarityGate` (real), `InMemoryClarityGate` (test double)

2. **Preflight Check** (`crates/preflight-check/`)
   - Validates code against semantic rules (not just grep patterns)
   - Multi-provider: Anthropic, OpenAI, Ollama
   - Exit codes: 0=pass, 1=violated, 2=error
   - Example: "BDD-outer: acceptance tests were RED before implementation"

---

## Design Gaps

**From RFC-01 (Status: Draft):**

The KB describes a **3-layer Front Door architecture** that is NOT BUILT:

1. **Sonnet Router** — lightweight human interface, shows state, accepts intent → **NOT BUILT**
2. **Pre-Processing Gate** — clarity + context enrichment + route decision → **PARTIALLY BUILT** (clarity_gate exists, router does not)
3. **Agency Cluster** — execution (Lead-dev, Coders) → **EXISTS**

Also: **Front Desk ≠ Agency Gates**. Two trajectories:
- **New project:** Front Desk → Agency Gates → Build State → Conversion → Kanban
- **Onboarded:** Front Desk → Execution → Kanban

---

## Current Status

- `ClarityGate` and `preflight-check` exist as code
- **Missing:** Unclear if clarity_gate is called in any daemon (wiring incomplete)
- **Missing:** Sonnet Router frontend
- **Missing:** Context enrichment gate

---

## Portage Sequence (RFC-01)

1. Document what clarity_gate actually validates (currently code-only, no spec)
2. Decide: is the Sonnet Router / ContextEnricher design still desired, or does clarity_gate + preflight-check suffice?
3. Wire clarity_gate into the intake flow

---

## Why Interesting for v2

Cheap LLM validation before heavy work = cost savings + faster feedback. The gate also enforces that brief references are real (not hallucinated issue numbers, paths, etc.). This is the first filter before the expensive pipeline.

