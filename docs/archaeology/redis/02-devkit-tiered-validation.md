# DevKit — Tiered Validation Pipeline (ideas worth porting)

**Source keys:**
- `devkit:archaeology:architecture-blueprint`
- `devkit:archaeology:tier0-quality-engine`
- `devkit:archaeology:event-driven-architecture`
- `devkit:archaeology:component-inventory`
- `devkit:archaeology:integration-gaps`
- `devkit:archaeology:refactoring-blueprint`
- `devkit:archaeology:evolution-timeline`
- `devkit:archaeology:next-actions`
- `devkit:archaeology:source-of-truth-docs`
- `devkit:archaeology:master-index`

## Why interesting for v2
DevKit = "methodology compliance proof system." The 4-tier validation pattern + event hooks are genuinely novel and fit v2's "skills as structural orchestrator constraints" directive. BUT: implementation is PHP/Symfony + Rust and 27K LOC — user wants "avoid creating too much code." Distill the ideas, leave the code.

## Core principle (verbatim)
> DevKit is a METHODOLOGY COMPLIANCE PROOF SYSTEM, not a code quality checker.
> Validates: "Was BDD→Contract→TDD→Reality followed?"
> NOT validating: Algorithm quality, performance, naming

## 4-Tier Validation (cost-optimised)
```
Tier 0: Quality Recipe Engine    FREE    <1s     Pre-filter (grep/AST on YAML recipes)
Tier 1: Physical Evidence        FREE    <100ms  Files, git diff, gherkin parse → EvidenceBundle
Tier 2: Fast Model Heuristics    $0.006  <2s     Haiku, 90% pass rate
Tier 3: Premium Judge            $0.35   <5s     Gemini/Opus, only 10% escalation
```
Cost: $35/day → $0.82/day (98% savings) when Tier 0 catches 60%+ early.

## Recipe-based (external, YAML, composable) — KEY INSIGHT
> Quality checks run BEFORE methodology validation.
> Why: "Why validate methodology on messy code? Judge NEVER called if quality fails."

Example recipes:
| Recipe | Detection | Blocks on |
|---|---|---|
| unwrap_detector.yaml | grep `\.unwrap\(\)` | max: 0 |
| complexity_threshold.yaml | ra-query | >15 |
| clone_detector.yaml | grep + context | unused clones |
| layer_violations.yaml | dep graph | sqlx/reqwest in ports |
| faithful_doubles.yaml | grep param usage | doubles ignore inputs |

Per-project config picks which recipes apply per gate per project.

## Event-Driven Extensibility — 5 hooks per gate (KEY INSIGHT)
> Every gate is event-driven. A0 or lead-dev can hook into any step without modifying core code.

1. `devkit.gate.before` — block gate pre-execution
2. `devkit.evidence.collected` — augment evidence (add custom scans)
3. `devkit.validation.before` — pre-validation checks
4. `devkit.verdict.before` — override verdicts, add gaps
5. `devkit.gate.after` — metrics, notifications (read-only)

Symfony EventDispatcher underneath. Listeners are tagged services in services.yaml.

This is the "tool-gating via events" pattern. Very valuable for v2's "tools configurable per agent-type" directive.

## 4 Core Gates (names to steal, impl to throw away)
- SPEC (BDD Intent): gherkin parsed, hermetic scenarios, domain language
- CONTRACT (Port Purity): trait + InMemory + zero port leaks + domain outcomes (not bool)
- DOMAIN (TDD Proof): RED→GREEN proofs, clippy clean, complexity <15, zero unwraps
- INTEGRATION (Reality Check): real adapter, integration tests, **side-effect proof (redis-cli MONITOR, psql logs)**

## State machine enforcement
> - Cannot skip gates (HARDSTOP)
> - Cannot advance until gate PASSES
> - Max 10 iterations per gate → escalate to human
> - Loop-until-clear mechanics (not single-pass)

## Architect review of RFCs (from 2026-04-19)
From `memory:feedback:architect-review-reads-existing-docs`:
> Every `Agent(subagent_type=...)` call invoking an architect for RFC review MUST include a 'Required prior reading' section enumerating by path: prior RFCs, council verdicts, CLAUDE.md, specs, open issue backlog, sibling-repo context.
> Verdict without citations is automatically 'REQUEST CHANGES: re-review with required-reading loaded.'

This is the "councils use agent teams, not sub-agents" rule in CLAUDE.md §2b, but the required-reading preamble is the load-bearing bit for v2's judge agents.

## What NOT to port (caution)
- PHP/Symfony stack (user wants Rust-first? unclear)
- 27K LOC of devkit-server + 15K LOC of devkit-gates
- Named "judges" (Watson, Inquisitor, Sherlock) — cute but arbitrary
