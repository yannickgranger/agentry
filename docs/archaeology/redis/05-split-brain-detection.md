# Split-Brain Detection — Hard Lesson from qbot-core

**Source keys:**
- `kb:split-brain:pattern-taxonomy`
- `kb:split-brain:prevention-checklist`
- `kb:split-brain:param-registry-2782`
- `plan:qbot-core:concept-graph-automation`

## Why interesting for v2
qbot-core shipped 1000+ issues and the dominant bug class became split-brain: two resolvers, two alias maps, two state machines for one concept. v2 orchestrator will supervise MANY repos. It MUST enforce split-brain-free code across all of them or the pattern replicates 10×.

## Pattern taxonomy (learned from real bugs)

### 1. Consumed-but-unregistered
> Param consumed via `.get('key')` in resolver/factory but missing from ParamRegistry. MCP rejects, CLI works.
Prevention: architecture test scanning .get() calls against registry (backward invariant).

### 2. Registered-but-unconsumed
> Param in registry but never consumed by any resolver. Users see it in help, set it, but it has no effect.
Prevention: liveness test — scan consumer files, verify every ParamDef appears.

### 3. Parallel-resolution (MOST COMMON)
> Same concept resolved by 2+ independent functions that diverge over time.
Prevention: "Single resolution brain" pattern — one canonical resolver per concept.

### 4. Alias-map-divergence
> Two independent alias tables that diverge when one is updated.
Prevention: Registry as single alias source.

### 5. Entry-point-asymmetry
> Different entry points (MCP/CLI/Portfolio) take different code paths, one works and another doesn't.
Prevention: `all_entry_points_produce_same_engine_config` test.

## The devastating insight
From `kb:split-brain:param-registry-2782`:
> **You cannot overcome LLM locality bias with prose rules. Only a failing test or compile-time link breaks the false-success cycle.**

Ranked countermeasure effectiveness:
1. Failing tests (strongest)
2. Architecture change
3. Skill enhancements
4. Prescription templates
5. Prose rules (weakest — failed 3x)

Bias sources named:
- **Locality bias**: LLM attention favors co-located code. Registry in different file/module/crate is invisible during resolver editing.
- **Completion bias**: Tests pass without registry entry → false-success signal stops work.
- **Task-boundary bias**: PRs classified as 'factory-only' narrow scope, making registry feel out-of-scope.

## v2 Implications
The orchestrator cannot enforce "don't duplicate" with prompt engineering alone. v2 must:
1. Run cfdb (or equivalent) as a STRUCTURAL gate at PR time.
2. Wire "canonical resolver" check into /prescribe — not "should" but "must verify".
3. Every CREATE decision must pass the "split-brain test" from CLAUDE.md §4.

## Concept Graph — human-in-the-loop, not automated
From `plan:qbot-core:concept-graph-automation`:
> Automated /prescribe integration has 60-70% ceiling due to sub-agent bypass bias and hallucinated justifications.
> Human-driven audit avoids the bypass problem entirely by keeping a human in the interpretation loop.
> Graph becomes 'git grep for semantic concepts' — a tool reached for, not a gate to satisfy.

Model: weekly full re-ingest + canned audit report (split-brain sweep) read Monday morning by human. Not an automatic gate. Human interprets, files issues.

v2 lesson: don't over-automate analysis. Let the tool make findings, let the human judge. "graph-specs check" is a gate (binary pass/fail). Classifier verdicts are advisory.
