# Super Green TDD: 2-Step, Not 3-Step (Eliminates Refactor Debt)

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/site/src/reference/agent-protocols.md`
- `/var/mnt/workspaces/agency-orchestrator/site/src/reference/process-evolution.md`

**1-line summary:** Reject the RED → GREEN → REFACTOR pattern. Use RED → SUPER GREEN (clean code first time). Refactor phase creates bad habits; Super Green enforces discipline.

---

## The Critique

Traditional TDD: "Make it work (GREEN), then make it right (REFACTOR)."

**Problem:** Encourages sloppiness. Coders write minimal code to pass fast, rationalize "fix it later" (which never happens). Technical debt accumulates.

From agent-protocols.md:

> "Super Green code must pass all gates immediately: cargo fmt --check, cargo clippy -- -D warnings, cargo test."
> 
> "If you need a refactor phase, you didn't think hard enough before coding."

---

## Two Phases (Not Three)

### Phase 1: RED

Test that **fails on behavior** — NOT compilation.

**RED is NOT:** compilation errors, missing imports, undefined types, syntax errors. Those are Wishful Thinking API Discovery (Phase 0, unstated but valuable).

**RED IS:** code compiles, test runs to completion, assertion fails on expected behavior.

### Phase 2: SUPER GREEN

Write the implementation **correctly the first time**. 

- Clean, readable code with proper error handling
- Correct ownership patterns
- No TODO comments
- No "fix later" hacks
- Passes all gates immediately

---

## Contract-First Sequence (BDD-TDD-Contract Flow)

```
BDD OUTER LOOP (Given/When/Then from acceptance criteria)
  CONTRACT LAYER (Trait defines behavior, tested against InMemory + Real)
    TDD INNER LOOP
      RED ---------> SUPER GREEN
      (assertion     (clean code
       fails)         first time)
```

**Steps:**
1. Define trait in domain/ports
2. Write contract test against trait (RED)
3. Create InMemory double (passes contract test)
4. Use double in unit tests (TDD inner loop, RED → SUPER GREEN)
5. Implement real adapter (passes same contract test)
6. Run BDD scenario with real infrastructure

---

## Metrics (Proof from Early Sprints)

| Metric | Before Super Green | After Super Green |
|--------|-------------------|-------------------|
| Rework rate | ~30% | Near 0% (gate-enforced) |
| Code review cycles | 3-4 | 1 (gates guarantee quality) |
| Refactor PR frequency | High | Minimal |

From process-evolution.md:

> "The fundamental learning: quality became the path of least resistance. When gates are the only way to declare 'done,' agents follow methodology perfectly."

---

## Implementation in Methodology

- **Gate Sequence:** BDD (acceptance tests RED) → Contract gate (InMemory + Real pass) → TDD gate (unit tests SUPER GREEN) → Integration → Ship
- **Tool enforcement:** coder-mcp-server blocks `write_impl` until prior gates pass
- **Agent framing:** "Super Green code earns SUPERGREEN state immediately"

---

## Why Interesting for v2

This flips "code quality is a tax" to "quality is the path of least resistance." By refusing the refactor phase, you force agents to think before coding. The contract-first pattern ensures correctness early (InMemory double proves the design works before real implementation). Proven: 30% → 0% rework.

