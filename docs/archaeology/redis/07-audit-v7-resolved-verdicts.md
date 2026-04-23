# Audit v7 — Resolved Verdicts (what not to re-question)

**Source keys:**
- `audit:v7:plan`
- `audit:v7:phase3:architecture-fundamentals`
- `audit:v7:phase3:agent-lifecycle`
- `audit:v7:phase3:docker-adapters`
- `audit:v7:phase3:front-gates`
- `audit:v7:phase3:gatekeeper`
- `audit:v7:phase3:gate-registry`
- `audit:v7:phase3:coder-stdin-daemon`
- `audit:v7:phase3:theme1-ghost-repos`
- `audit:v7:phase3:theme2-php-rust`
- `audit:v7:phase3:aegis`
- `audit:v7:phase3:CR-004`

## Why interesting for v2
A v7 audit ran 2026-03-08, cataloged 19 repos + 63 crates + 42 contradictions, and resolved 30 of them with the user. These are FROZEN decisions — v2 must not re-open them.

## Stats (verbatim)
> Orchestrator Audit v7 — COMPLETE 2026-03-08
> 19 repos (13 live, 5 dead, 1 duplicate)
> 16 binaries
> 63 orchestrator crates
> 24 other crates
> 42 streams (15 orphaned)
> 15 frozen decisions
> 15 known gaps
> 15 cross-repo deps

## Architecture fundamentals — DO NOT RE-QUESTION
From `audit:v7:phase3:architecture-fundamentals` (verbatim):
> 1. agency-control is the consumer/frontend (currently decommissioned, will be rebuilt)
> 2. A0 is NOT lead-dev. A0 is the architect. Lead-dev is a separate role (daemon-managed Claude process). **NEVER conflate them.**
> 3. devkit-server (PHP) and agency-orchestrator (Rust) are separate systems — not competing implementations
> These facts have been confirmed 7+ times across audits. DO NOT re-question them.

## Front-gates before intake
From `audit:v7:phase3:front-gates` (verbatim):
> CRITICAL ARCHITECTURE FACT: The system uses FRONT-GATES BEFORE INTAKE.
> There is no simple "intake model" — tasks go through gates before they enter the pipeline.
> This has been missed by every audit. DO NOT assume a simple intake flow.

Contradicts user's v2 shape somewhat — user said v2 intake is "brief from a front-door agent" (single entry). Could be reconciled: the front-door agent IS the gate.

## Gate registry, not enum
From `audit:v7:phase3:gate-registry` (verbatim):
> Gates are a PROGRAMMATIC REGISTRY, not a hardcoded enum.
> A0 configures which gates are active per issue (gate setup/profile).
> Gates activate dynamically depending on what the coder does.
> Neither the PHP 11-gate enum nor the Rust 4-gate enum is "correct" — both are wrong.
> The correct model is a registry that A0 populates and the system enforces mechanically.

v2 should model gates as **data + registry**, not enum cases.

## Gatekeeper → gitless agents
From `audit:v7:phase3:gatekeeper` (verbatim):
> Gatekeeper (ship-authorize) solves the wrong problem. The real fix is making agents GITLESS — no git in their deployment at all.
> KEEP for now as a stopgap, but the RFC target is gitless agents.

v2 directive: design agents gitless from day zero. No setuid binaries, no push-authorization dance.

## Docker adapters stay
From `audit:v7:phase3:docker-adapters` (verbatim):
> Docker/Podman was dropped as default because it lacked features compared to LXC/Proxmox.
> But an orchestrator should support multiple infra backends (Docker/Podman, LXC, K8s).
> Proxmox/LXC is the current default, Docker support stays as an option.
> KB "superseded" language is wrong — Docker is deprioritized, not removed.

Matches user's v2 directive: "Substrate user-chosen: LXC, Docker, VM, Alpine+ssh, Docker+ssh. Dev = podman."

## PHP + Rust coexist by design
From `audit:v7:phase3:theme2-php-rust` (verbatim):
> No contradiction. Orchestrator core is Rust. devkit-server is PHP.
> They are TWO SEPARATE SYSTEMS, not dual implementations of the same thing.
> Different stream names are expected — they serve different purposes.
> DO NOT flag PHP/Rust coexistence as a conflict — it's the architecture.

Relevant for v2: user's v2 is "model-agnostic, agent-agnostic" — different languages for different subsystems is fine.

## coder-daemon depends on stdin-daemon (as lib)
From `audit:v7:phase3:coder-stdin-daemon` (verbatim):
> stdin-daemon is the base agent library (Claude CLI bridge).
> coder-daemon SHOULD depend on stdin-daemon as a library and add guidance.rs on top — not duplicate the entire repo.
> Current state: full copy with identical crate names.
> DECISION: coder-daemon keeps its own repo but imports stdin-daemon as a dependency.

Clean-arch note: stdin-daemon is the template for "claude CLI bridge." v2 can reuse it.

## AEGIS work permits — future IAM
From `audit:v7:phase3:aegis` (verbatim):
> AEGIS (work permits) is a FUTURE feature for IAM. Implemented + 34 tests, zero consumers currently. KEEP — do not remove.
> Will be wired when IAM layer is built.

Matches user's v2 directive: "All tool use MONITORED. Session + IAM identity per agent." AEGIS is the IAM layer waiting for activation.

## Ghost repos — resolved dead
From `audit:v7:phase3:theme1-ghost-repos`:
> Ghost repos (agency-streams, agency-terminal, mcp-devkit standalone, agency-libs, agency-archi) were removed/abandoned intentionally.
> KB references are stale. Do not rescan.

Don't pull code from these repos. They are dead by choice.

## agency-control is decommissioned
From `audit:v7:phase3:CR-004`:
> agency-control intentionally decommissioned by user. Was a Symfony app with broken features, user scrapped it.
> Status: DEAD (by choice)

User's v2 directive mentions dashboard — but do NOT resurrect agency-control. Start from scratch.
