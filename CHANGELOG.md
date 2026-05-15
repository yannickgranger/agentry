# Changelog

All notable changes to agentry. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] — 2026-05-15

The v2 release. Closes the FSM phase-enum split-brain by making the brief
lifecycle topology-driven end-to-end.

### Changed — breaking

- **Topology-driven lifecycle (epic #397).** `BriefState` is now
  `Walking { node_id, evidence, retry, run_data } | Shipped | Failed { reason }`.
  Methodology phase enums (`Verifying`, `Reviewing`, `Shipping`, `Watching`,
  `Reworking`, `AwaitingCaptainDecision`) are gone from the type surface; the
  team topology JSON is the single source of truth for fan-out / fan-in /
  gate policy / rework targets. Adding a new role-class no longer requires a
  Rust code change.
- **`RunData` rewritten** to carry only data the topology declares it needs
  (`pr_number`, `head_sha`, etc.) rather than per-phase variants.
- **Daemon `handle_brief` mini-FSM collapsed** (#539). The in-process role-
  chain shadow (`all_messages`, `overrides_for`, `reworks_used`,
  `shipped_roles`) is deleted; every read goes through `BriefState::Walking`
  and the per-brief trace stream. The trace + topology + `BriefState` triple
  is now the only state the daemon consults at brief-walking time.
- **`Cargo.toml` `repository`** now points at the github mirror
  (`https://github.com/yannickgranger/agentry`) rather than the maintainer's
  private forge.
- **Default `forge_host`** in production fallbacks is `forge.example.com:3000`
  rather than the maintainer's private forge. Operators must set
  `[forge] default_host` in `agentry.toml` (see `agentry.example.toml`).

### Added

- **CFDB ban rules** enforcing the topology-driven lifecycle:
  `arch-ban-briefstate-variants` (#540), `arch-ban-rundata-phase-names`
  (#541), `arch-ban-phase-name-strings` (#542). Eight ban rules total run
  on every PR via `scripts/arch-check.sh`.
- **FSM-settled barrier** (#539 phase 7a/7b). The daemon awaits a stable
  `BriefState` projection before issuing terminal verdicts, removing the
  shipped-roles/reworks-used in-process counters.
- **Dashboard Recent-briefs panel** (#539 phase 5b). Persistent clickable
  history of recent briefs with terminal verdicts.
- **README operator docs** for `orchestrator agents list`, `agents query`,
  `recent status`, and `abort`.
- **Captain freshness gate v1** (#502) — file:line-level freshness check
  on brief acceptance.
- **`scripts/captain-redeploy.sh`** tracked in the tree (#7aa54db).
- **CI watcher enriched failing-checks** payload (#508) — full list of
  failing CI contexts surfaced to reviewers.

### Fixed

- **Daemon resume race after #559** (#562). `read_walking_view`
  discriminates terminal vs `Submitted` so the 5s resume barrier doesn't
  silently swallow trivial-doc briefs.
- **`await_fsm_settled` timeout budget** (#559). Higher budget + fail-on-
  timeout so the resume barrier surfaces stalls instead of timing out
  silently.
- **Captain dispatch canonical config** (#501) — captain reads the operator's
  `agentry.toml`, not its own.
- **Captain `new` acceptance cross-repo guard** (#500) — refuses dispatch
  when the captain's CWD repo doesn't match the brief's `target_repo`.
- **Daemon orphan detector** (#503) — stale-trace probe for orphaned briefs.
- **Mutex/RwLock poison recovery** (#brf_debt_002) — daemon survives a
  panicked-thread poisoned lock instead of cascading to terminal.
- **HTTP timeouts on forge calls** (#brf_debt_003) — bounded request
  duration so a stuck forge can't hang a brief indefinitely.
- **Translator routing** (#529) — daemon yields routing to the FSM rather
  than disagreeing with it; lifecycle owns the decision.

### Removed

- All methodology-phase string literals from `crates/orchestrator-runtime`
  and `crates/orchestrator-types` (enforced by `arch-ban-phase-name-strings`).
- `PhaseGates` struct (subsumed by per-edge `GateConfig` in topology JSON).
- The pre-#330 hardcoded `agency.lab` / `agentry-sccache-redis` literals
  in `seed.rs` (now derived from `Config`).

### Infrastructure

- **Tech-debt sweep** (briefs #001-009): per-brief `#[tracing::instrument]`,
  Cargo workspace inheritance, dedup of `redis::Value::as_str` helpers,
  workspace-allocate-at-extract fix, captain-freshness extraction.

## [0.1.0] — 2026-05-04

First OSS-ready release. Minimal orchestrator for ephemeral agent
containers; NDJSON stdin/stdout protocol; daemon reads `Brief`s off
`agentry:briefs`, resolves a `TeamTopology` + `AgentRole`s, mints signed
`WorkPermit`s, spawns one container per role, enforces the permit on
every `tool_call`, routes inter-role `Message` events, records a verdict.

[Unreleased]: https://github.com/yannickgranger/agentry/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/yannickgranger/agentry/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/yannickgranger/agentry/releases/tag/v0.1.0
