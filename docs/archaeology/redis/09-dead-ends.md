# Explicit Dead Ends — What v2 MUST NOT Repeat

## Why interesting for v2
Every entry is a decision proven wrong or a pattern user explicitly rejected. Do not rebuild these.

## 1. Reviving v1 agency-orchestrator
**Source:** `project:orchestrator-v2:shape`
> Do NOT revive the existing orchestrator. Start fresh with: Top-down minimalistic architecture.
> The existing agency-orchestrator (62 crates, 30 open / 0 closed issues) is dead.

62 crates, cross-crate ACL translators, 30 open / 0 closed issues.

## 2. 20 RFCs before any code
**Source:** `project:orchestrator-v2:shape`
> One RFC per component, written WHEN building that component (not 20 RFCs upfront)

## 3. Horizontal slices (types → wiring → behavior)
**Source:** `project:orchestrator-v2:shape` + CLAUDE.md §4 vertical slices rule
> A horizontal slice touches one layer across multiple features.
> This makes BDD, contract tests, and reality gates meaningless until the final issue.

## 4. InMemory doubles as tests
**Source:** `project:orchestrator-v2:shape`
> InMemory doubles pretending to be tests
> 11 of 23 provider traits have ONLY InMemory adapters — those are suspect
> v2: BDD against real infrastructure from day one — ZERO InMemory doubles

## 5. Gatekeeper / setuid push-authorization binaries
**Source:** `audit:v7:phase3:gatekeeper`
> Gatekeeper (ship-authorize) solves the wrong problem. The real fix is making agents GITLESS.

## 6. Manual SHA ceremony between paired repos
**Source:** `memory:feedback:no-manual-sha-ceremony` + `feedback:cfdb:no-sha-dance`
> User reaction: "I DONT GIVE A SHIT. DONT WANT TO MANUALLY CHANGE SHA EVERY 30 MIN"
> Pin ONCE at PR creation to branch HEAD. No mid-merge updates.
> Better: develop-to-develop with no pin at all.

## 7. File-based memory + MEMORY.md index
**Source:** `memory:policy:redis-only` + `feedback:cfdb:memory-in-redis-not-files`
> DONT USE MEMORY. redis mcp_memory. NO FILE MEMORY. REMOVE PREVIOUS FILES
> Redis-only. Never create .md memory files.

## 8. SQLite for terminal/UI state
**Source:** `agency-terminal:lean-architecture`
> Removed complexity: SQLite → JSON file
> Atomic tempfile+rename replaces SQL for UI state.

## 9. 60 FPS render debounce
**Source:** `agency-terminal:lean-architecture`
> Removed: 60 FPS debounce → use existing 100ms poll

## 10. Hard input lock during streaming
**Source:** `agency-terminal:lean-architecture`
> Removed: Hard input lock → soft lock with buffer
> No grayed-out frustration, queued feel.

## 11. Backlog inflation (file issue for every finding)
**Source:** `feedback:cfdb:no-backlog-inflation`
> Don't default to filing new tracker issues for drift.
> User: "we wont grow backlog to infinity"
> Prefer fix-in-place (≤15 min boy-scout), inline PR note, or silent leave-alone.

## 12. Isolated architect review (no required reading)
**Source:** `memory:feedback:architect-review-reads-existing-docs`
> Reviewing in isolation produces verdicts that duplicate or contradict prior decisions.
> Verdict without citations → auto-REJECTED.

## 13. Automated /prescribe concept-graph gate
**Source:** `plan:qbot-core:concept-graph-automation`
> Automated /prescribe integration has 60-70% ceiling due to sub-agent bypass bias and hallucinated justifications.
> Pivoted to human-driven audit: weekly report, human interprets.

## 14. Parallel sub-agents for council work
**Source:** CLAUDE.md §2b + `memory:session:2026-04-19-cross-dogfood-ratify`
> Never emulate a council with parallel Agent(subagent_type="...") calls.
> Use TeamCreate. The team machinery is load-bearing.

**NUANCE:** for architect DECOMPOSITION of a single issue, parallel Agent() calls work (4-lens concurrent review of an EPIC). For deliberation/contested resolution, use TeamCreate. Different use cases.

## 15. Metric baseline/ceiling files
**Source:** CLAUDE.md §6 rule 8
> No metric ratchets. Every metric gate is zero-tolerance against a hard threshold defined as a const in tool source.
> No ceiling file, no allowlist, no transitional waiver, no --update-baseline flag.
> Any PR that proposes adding a baseline/ceiling/allowlist is rejected on sight.

## 16. Over-automated dashboards / dashboard-before-engine
**Source:** `architecture:observability`
> status: DRAFT - backend not finished
> Designed before core integration complete. Revisit after M1-M4.

Dashboard first, but engine has to actually produce signal first. Don't spec observability for nothing.

## 17. Prose rules as split-brain prevention
**Source:** `kb:split-brain:param-registry-2782`
> You cannot overcome LLM locality bias with prose rules.
> Only a failing test or compile-time link breaks the false-success cycle.

v2: structural enforcement (CI gate) or nothing.

## 18. Multi-question OQ enumerations
**Source:** `feedback:graph-specs:one-question-no-admin`
> Ask user one question at a time in plain prose. No decision tables, no OQ enumerations.
> User called out "administrative giggle" after a 4-question list.

## 19. Ship-ready PR before architect review
**Source:** `feedback:graph-specs:no-pr-before-architect-review` + `wip-pr-pattern`
> When user says "PR the RFC", run four-lens architect-team review FIRST.
> Open as WIP (title prefix) until architects finish.
> User merged an over-engineered PR while simplification was still being amended in.

## 20. Named judges as personas (Watson, Inquisitor, Sherlock)
**Source:** `devkit:archaeology:source-of-truth-docs`
> Quality Enforcement Bible: Audit personas (Watson, Inquisitor, Sherlock)

Cute but arbitrary. v2 doesn't need personified judges. Name gates by what they check (spec, contract, domain, reality).
