# Orchestrator v2 Shape — The Keystone

**Source keys:** `project:orchestrator-v2:shape`

## Why interesting for v2
This is the ONE key that defines what v2 is supposed to be. User-authored 2026-04-01. Read everything through this lens.

## Verbatim — Core doctrine
> After 1000+ qbot-core issues, the methodology is proven but lives as prose rules in CLAUDE.md.
> The existing agency-orchestrator (62 crates, 30 open / 0 closed issues) is dead — too much untested InMemory code, too many horizontal slices, too much planning without shipping.

## Decision
> Do NOT revive the existing orchestrator. Start fresh with:
> - Top-down minimalistic architecture
> - Extract/project existing code INTO new components where it fits
> - Each component gets: Excalidraw diagram + RFC + EPIC (linked)
> - Vertical slices only — each issue delivers testable behavior E2E
> - BDD against real infrastructure from day one — ZERO InMemory doubles
> - Proper methodology/skills for each issue

## System shape
> Two things at minimum:
> 1. **Engine** — orchestrator core (components TBD by user)
> 2. **Dashboard** — consumes the engine, deployed to staging on every push to develop
>
> The dashboard forces real paths to be exercised. No InMemory lies survive a deployed consumer.

## Core loop to automate
> issue → discover → prescribe → code → gates and judges → ship
>
> What exists today as Claude Code skills (/discover, /prescribe, /gate-*, /ship) should become **structural orchestrator constraints — things the agent CANNOT bypass**, not rules it SHOULD NOT bypass.

## Principles reinforced verbatim
- "No assumptions from Claude — user defines domains, Claude writes them down"
- "Claude's CREATE bias killed v1 — every business rule assumption translated to code against user's will"
- "User has no Rust expertise to catch Rust-specific biases — modules must be independently testable"
- "One RFC per component, written WHEN building that component (not 20 RFCs upfront)"
- "Dashboard-first development — if the engine works, the dashboard proves it"

## Worth evaluating from v1 (user's own triage list)
- stdin-daemon: "TEMPLATE for how modules should look" — clean, focused, deployed
- agency-bus: typed Redis Streams protocol, functional
- mcp-forge: 16 MCP tools, RBAC, functional
- mcp-rules: mature (v0.4.0), functional
- devkit-server (PHP): 100% P0 done, 3-tier LLM judges, 14 endpoints
- quality-recipes: Tier 0 YAML-based mechanical checks, wired
- forge-contracts: shared VCS abstraction
- PHOSPHENE: 85% core complete, Bayesian fusion, mood FSM
- **11 of 23 provider traits have ONLY InMemory adapters — those are suspect**

## Anti-patterns (user-authored "do not repeat")
- 20 RFCs before any code
- 62 crates in a monorepo with cross-crate ACL translators
- Horizontal slices (types → wiring → behavior)
- InMemory doubles pretending to be tests
- Claude inventing business rules
- Planning without shipping (30 open, 0 closed)

## Sequencing (user-authored)
> 1. qbot-core first — earn money/time
> 2. Orchestrator when ready — build faster
> 3. When starting: new repo, diagrams + RFCs + epics, dashboard-first, real infra from day one
> 4. User defines domains, Claude draws and writes
