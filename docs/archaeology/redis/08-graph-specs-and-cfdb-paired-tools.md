# Paired Tools — cfdb + graph-specs (model for v2 quality tooling)

**Source keys:**
- `memory:project:graphspecs-rust`
- `memory:session:2026-04-19-cross-dogfood-ratify`
- `memory:session:2026-04-19-hir-keystone-decomposed`
- `memory:session:2026-04-19-phase1-rescue-kickoff`
- `plan:qbot-core:top-3-epics-2026-04-21`

## Why interesting for v2
User built TWO tools that work together: cfdb (code-facts DB) = X-ray (detect debt); graph-specs = Vaccine (prevent drift via PR gate). The "paired tool" pattern with "cross-dogfood" is the quality ideal for v2.

## The division of labor (verbatim)
> cfdb (code-facts DB) = X-ray, detect existing debt, runs on-demand audits + debt triage.
> graph-specs = Vaccine, prevent new drift, runs every PR in CI.
> Use cfdb to clean a context up, then spec-lock the cleaned context in graph-specs so it stays clean.

## Four levels of equivalence (graph-specs)
> (1) Concept — every named concept in specs exists as a type in code and vice versa.
> (2) Signature — every port trait in specs matches the Rust trait signature.
> (3) Relationship — every implements/depends on/returns edge in specs exists in code's graph.
> (4) Bounded context — every named context maps to exactly one crate set; no type crosses a boundary unless spec declares the crossing.

Mechanism is **pure mechanical parse-and-compare, no LLM, no fuzzy matching.**

## The spec dialect (markdown → concept graph)
> Markdown reader parses ##/### headings → concept nodes
> fenced rust blocks reserved for signature extraction
> `- implements: X` / `- depends on: X` / `- returns: X` bullets for relationship extraction
> Rust reader parses top-level `pub struct`/`pub enum`/`pub trait`/`pub type` only — ignores non-pub, #[cfg(test)], nested mods, impl/fn/const/static

Very constrained but deterministic. Perfect for a mechanical gate.

## Cross-dogfood = locked facts, NOT SHA pins
From `feedback:cfdb:cross-dogfood-is-fact-locking`:
> The primitive is the bytes-on-disk snapshot of the facts MY tool extracts from the companion's code, not a pointer to which commit of the companion I looked at.
> The locked facts file IS the audit trail.

Right shape: `.cfdb/cross-locked.json` containing facts snapshot. Tool provides `lock-cross` (regenerate) and `verify-cross` (regenerate + diff) subcommands.

Wrong shape: SHA pin + weekly bump cron + lockstep PR mandate. "Humans doing what the binary should do."

## RFC→Spec→Issues→Impl pipeline
From `memory:feedback:workflow-rfc-spec-issues-impl`:
> Every non-trivial change follows: architect writes RFC → derives spec → files issues → implementation PRs close issues and update spec in same commit.
> Never skip forward. Every issue traces to a specific RFC section.
> Orphan issues (filed without backing RFC) are drift.

**Stage boundaries are physical:**
- `docs/` — RFCs + council verdicts (not machine-parsed)
- `specs/concepts/` — machine-parseable contract (graph-specs reads)
- Issue bodies MUST cite RFC section
- PR closes issue AND updates spec in same commit — never code-only, never spec-only

## Tier-1 binary + Tier-2 agent (split)
From `plan:qbot-core:top-3-epics-2026-04-21`:
> cfdb check-predicate --db <path> --keyspace <name> --workspace-root <path> --name <predicate>
> Exit non-zero iff ≥1 row (zero-tolerance)
> --no-fail for informational mode

Tier-1 = deterministic binary that outputs JSON. Tier-2 = agent (LLM) that interprets semantics. Clean separation.

## Architect team decomposition (proven 2026-04-19)
From `memory:session:2026-04-19-hir-keystone-decomposed`:
> 4-architect parallel review (Agent calls) produced convergent findings in ~1-2 minutes total.
> Cost: 4 agents × ~100k tokens each ≈ 400k tokens for decomposition of a keystone issue.
> Cheap insurance against shipping a bad split.

Lenses used: clean-arch + ddd-specialist + solid-architect + rust-systems.

Every prompt carried "Required prior reading" with file:line anchors (drift prevention).

## Drift-lock recipe per child issue
Every child issue of a decomposed epic includes:
1. Required prior reading (file:line citations)
2. AC with citation-backed claims
3. Explicit "does NOT do" exclusions — prevents scope creep
4. Drift tripwires — verifiable greps/builds that MUST be true at merge (e.g. `cargo tree -p cfdb-cli | grep ra_ap` empty)
5. Mandatory pre-merge architect gates (specific agent + specific question per slice)
6. Tests table — Unit / Self-dogfood / Cross-dogfood / Target-dogfood rows
7. SchemaVersion lockstep with paired graph-specs-rust PR where applicable

This is a **template** v2 should steal for how to decompose an EPIC into child issues.

## The 7-Lens Council (from CLAUDE.md §2b)
Related pattern: `/pre-council` uses TeamCreate (NOT parallel Agent calls). 7 lenses deliberate in shared team scope.
From CLAUDE.md:
> The team machinery is load-bearing — it's how the council's merge-rationale + contested-resolution semantics actually work.

For v2: councils/lenses = agent teams. NOT fan-out to individual sub-agent calls. Structural difference matters.
