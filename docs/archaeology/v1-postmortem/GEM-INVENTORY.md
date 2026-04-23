# Gems Inside the v1 Graveyard — Salvage Candidates

One-line each, focused on what did NOT rot. Complementary to miner #2's broader external-asset inventory.

| Crate | Role | Why it didn't rot |
|-------|------|------------------|
| `workflow-engine` (4,416 LOC, 81 unit tests) | Artifact-store + command-runner state machine | Only crate that passed an unaided architecture audit (`ARCHITECTURE_AUDIT_WORKFLOW_ENGINE.md`): pure domain, adapter pattern actually adapts, zero infra leaks — the template for what "clean" could have looked like. |
| `agency-aegis` (2,019 LOC) | Signed work permits + audit trail (AEGIS) | Tight, self-contained security primitive. Single clear job, clear permit model (#392), never bloated into a triad. Reusable as-is for v2 ephemeral-agent permissioning. |
| `agency-llm-client` (3,104 LOC) | Anthropic API client | Provider-focused, no methodology coupling; useful unchanged for model-agnostic v2 (swap endpoint → works with Grok/Gemini if HTTP-compatible). |
| `forge-subprocess` (2,565 LOC) | `gh`/`tea` subprocess forge adapter | Works today, no InMemory inheritance, actually used by production binaries — the opposite of the triad-crate anti-pattern. |
| `agency-meilisearch` (2,018 LOC) | Meilisearch indexing + search client | Thin wrapper, honest scope, tests against real infra, no ceremonial doubles. |
| `context-condenser` (2,519 LOC) | Context-window compaction | Delivered real behavior (PRs #351/#354/#355 wired it end-to-end). Monitors Claude context %, proactively condenses. Useful for v2 Claude-MAX-bounded agents. |
| `quality-recipes` (920 LOC, YAML-driven) | Tier-0 mechanical-check engine (lint/fmt/types) | Data-driven: rules are YAML, not Rust code. Closest thing v1 had to "methodology as config" instead of methodology-as-code. Port the pattern, drop the runtime. |
| `ci-log-parser` (620 LOC) | Structured parse of Rust/Node CI logs | Narrow, testable, no ceremony. Direct reuse for v2 CI recovery signal. |
| `agency-trigger` (4,657 LOC) | Heartbeat + XREAD consumer loop for Redis streams | Functional today, exercises real Redis Streams. Harvest the xread/xclaim/heartbeat code (audit 2026-01-25 flagged `parse_xread_response` complexity 22 — still fixable). |
| `agent-events` (1,401 LOC) | Typed event catalogue for agent lifecycle | Single canonical event vocabulary; if v2 goes model-agnostic, these event shapes are provider-neutral. |
| `agency-aegis` + `agency-workspace` pair | Permissioned workspace preparation | Together they form a reusable "spawn an agent with a scoped filesystem" primitive — exactly what v2 needs for LXC/Docker/SSH substrates. |

**Do NOT salvage:** the 5 stub adapters (`intake/methodology/quality/delivery/execution-adapters`), any `*-contracts/src/doubles/` modules (P3 root cause), `devkit-gates` (methodology-as-code, 20K LOC of enforcement runtime), the RFC suite itself (20 specs for code that already existed).
