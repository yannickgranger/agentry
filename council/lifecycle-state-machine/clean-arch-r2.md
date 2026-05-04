# Clean-arch lens — r2

**Council:** lifecycle-state-machine-council
**Issue:** #246 — substrate brief lifecycle state machine
**Lens:** clean-arch
**Date:** 2026-05-02
**Precedes:** r1 at `clean-arch-r1.md`

---

## §1 Acknowledged from other lenses

### From rust-systems-r1

**Accepted: Three verdict-emission paths in daemon.rs, not one.** My r1 identified the composition-root violation but did not enumerate the call sites. Rust-systems' §1.2 maps all three correctly (role-outcome path at daemon.rs:394, handler-error path at daemon.rs:157, DOL meta path). The FSM eliminates path 1, maps path 2 to `BriefEvent::AbortRequested`, and leaves path 3 unaffected. This is the correct surgical scope and I adopt it.

**Accepted: `composition_role: Option<String>` not `Option<CompositionRole>` in v1.** My r1 proposed the `Option<String>` placeholder for `BriefStateRecord.composition_role` on similar grounds. Rust-systems made the same call explicitly. Aligned.

**Accepted: `BriefStateProjector` as a concrete struct, not a port trait.** I proposed `StateProjector` as a port trait; rust-systems named the concrete struct `BriefStateProjector`. The difference is load-bearing: my r1 insisted on a trait so the FSM driver is testable without Redis. Rust-systems' position ("the transition function `handle` is pure and separately testable; `BriefStateProjector` is the async shell") partially agrees but frames testability differently — the pure function is the seam, not the trait. I revise my position on this below (see §2 N3-revision).

**Accepted: No `#[non_exhaustive]` on `BriefState` or `BriefEvent`.** Rust-systems N8. Correct. Exhaustive match errors on new variants ARE the safety net. If `#[non_exhaustive]` were added, every future extension variant would silently fall through to `_` arms and miss the forced-update signal. My r1 didn't cover this; I adopt it as load-bearing.

**Accepted: `AttemptBudgetCap` as a `pub const u32` in `orchestrator-types`.** Rust-systems N6 proposes `ATTEMPT_BUDGET_CAP` const + `TeamTopology.max_retries` as soft cap where team config may not exceed the const. This resolves the C3 question cleanly (see §2 below).

**Accepted: `BriefEvent` must not shadow `EventKind`; the projector is the translation boundary.** Rust-systems N4. The FSM's `handle` function does not consume `EventKind` variants. The projector translates `EventKind::Done { verdict }` + role-name context → `BriefEvent::CoderDone | ReviewerDone | ...`. My r1 implied this but did not name it explicitly. Adopted.

**Accepted: The two-concurrent-loop-per-brief architecture is concrete.** My r1 described the composition root violation but did not name the two-loop structure explicitly. Rust-systems §1.4 names it: the orchestrator task (current `handle_brief`, drives role fan-out) and the FSM projector task (new, reads trace events). This changes the port shape question (see §2 below).

### From solid-architect-r1

**Accepted: `BriefMetadata` as a separate type for mutable audit fields.** Solid's N3 — "no variant carries more than 3 fields; excess belongs in `BriefMetadata`" — is structurally sound. My r1's proposed `BriefState::Authoring { agent_id, started_at, attempt }` has 3 fields (started_at is derivable from `BriefStateRecord.at`), but solid is correct that `started_at` belongs on the record, not on the variant. I drop `started_at` from my proposed `Authoring` variant and adopt the 3-field cap as a structural rule.

**Accepted: Dedicated `lifecycle.rs` module within `orchestrator-types`.** Solid's N5. The FSM types have a different change driver than `Brief` and `Verdict`. A new `orchestrator-types/src/lifecycle.rs` module prevents a lifecycle-semantics change from forcing recompile of all `Brief` consumers. My r1 did not specify where within `orchestrator-types` the new types live; I adopt the module boundary.

**Accepted: `BriefEvent` is a narrow type (ISP seam).** Solid's N2. The FSM consumer translates only the subset of `EventKind` that causes transitions. `Finding`, `Log`, `ToolCall`, `Message` are opaque to the FSM. Solid's measurement (FSM needs 2 of 8 EventKind variants = 25%) is the right threshold analysis. Adopted.

**Accepted: Workspace GC (Case 6) is a separate concern from the FSM.** Solid's N6. The FSM emits a terminal-state signal; the workspace module acts on it via a hook. These have different change drivers. The FSM EPIC should not bundle workspace retention policy. I adopt this as a scope constraint.

**Partially accepted: `BriefMetadata` vs variant payload.** I accept the 3-field cap and the `BriefMetadata` separation as a structural rule. However, I do not accept that `agent_id` should leave `BriefState::Authoring` — `agent_id` is the phase identity for `Authoring`. It answers "where is the brief AND who is doing it?" which is the minimum identifying context for the state. The `attempt` counter is also load-bearing on every non-terminal variant because the FSM's budget check is stateless (it reads `state.attempt()`, it does not query Redis). Removing `attempt` from variant payload requires a Redis lookup inside the transition function — that breaks the pure-function invariant. I hold `attempt: u32` on every non-terminal variant.

**Not accepted: `Extension` variant with table-driven dispatch (solid's N4).** See §2 and §3.

---

## §2 Revised non-negotiables

### N1 — FSM transition function is pure (no I/O, no async) [HOLD]

Unchanged from r1. All three lenses agree. `fn handle(state: &BriefState, event: &BriefEvent) -> Result<BriefState, InvalidTransition>` with no async, no Redis, compilable in `orchestrator-types`. The `attempt` counter being in the variant payload is the mechanism that preserves this: the function reads `state.attempt()` from the enum data, not from a Redis GET.

### N2 — FSM types in `orchestrator-types/src/lifecycle.rs` [STRENGTHENED]

My r1 said "types in `orchestrator-types`." Adopting solid's module-boundary refinement: a new `crates/orchestrator-types/src/lifecycle.rs` module. Types: `BriefState`, `BriefEvent`, `BriefStateRecord`, `BriefMetadata`, `InvalidTransition`. The `BriefMetadata` addition comes from solid's r1 (separate type for mutable audit fields). The module is `pub mod lifecycle` in `orchestrator-types/src/lib.rs`, with individual types re-exported via `pub use lifecycle::{...}`.

Rationale for module (not crate): the dashboard and any future consumer imports `orchestrator-types` today. Splitting to a new crate before there are two consumers with divergent stability requirements is premature decomposition.

### N3 — Port shape revision: `EventSource` as a stream-yielding trait; `StateProjector` as a record-writing trait; concrete impls in `orchestrator-runtime` [REVISED]

My r1 proposed two port traits (`EventSource`, `StateProjector`). Rust-systems declined the trait form for `BriefStateProjector` ("it is a concrete struct"). I revise to a middle position:

**`EventSource` remains a port trait.** The two-concurrent-loop architecture (rust-systems §1.4) means the FSM projector task reads the trace stream independently of the orchestrator task. For this loop to be testable without Redis, it must receive events through an injectable boundary, not a direct `ConnectionManager`. The trait shape for the r2 proposal:

```rust
#[async_trait]
pub trait EventSource: Send + Sync {
    async fn next(&mut self) -> Result<Option<(BriefEvent, String)>, SourceError>;
}
```

The `String` in the tuple is the Redis stream ID of the consumed entry (needed to persist the cursor). Production impl: `RedisEventSource` (holds `ConnectionManager`, calls `XREAD BLOCK`). Test double: `VecEventSource` (drains a `Vec<BriefEvent>`). This is minimal — no `Arc<dyn>` in library code, the binary monomorphizes.

**`StateProjector` collapses into the concrete `BriefStateProjector` struct** (agreeing with rust-systems). The pure `handle()` function is already the testable seam for the FSM logic. The async shell (`BriefStateProjector::project(record: &BriefStateRecord)`) writes to Redis; this is tested via integration tests, not unit tests. There is no benefit to abstracting it when there is exactly one implementation and the FSM logic (the interesting part) is already injectable via `handle()`.

**Monomorphization, not `Arc<dyn>` (resolving C1).** Rust-systems did not contest this in r1. The daemon binary is the monomorphization site:

```rust
// orchestratord.rs (composition root):
let event_source = RedisEventSource::new(conn.clone());
let projector = BriefStateProjector::new(conn.clone());
daemon::run(cfg, event_source, projector).await
```

`daemon::run<E: EventSource>` takes the generic bound; the binary fixes the type. No `Arc<dyn>` needed. My r1 flagged this as CONTESTED; rust-systems' silence on it is agreement. `Arc<dyn>` would only be warranted if runtime adapter selection were needed (e.g. a `--dry-run` mode that switches to in-memory) — that is not a stated requirement.

### N4 — `BriefEvent` is a distinct type from `EventKind`; projector is translation boundary [HOLD, NOW SHARED WITH RUST-SYSTEMS AND SOLID]

All three lenses agree. The FSM's `handle` function takes `BriefEvent`, not `EventKind`. The projector translates. I hold this as a non-negotiable and note it is uncontested across all three r1 filings.

### N5 — `parent_brief_id: Option<BriefId>` and `composition_role: Option<String>` on every `BriefStateRecord` from day one [HOLD]

Unchanged from r1. All three lenses include these fields. The field is load-bearing for composition — omitting it on day one forces a breaking migration on every `BriefStateRecord` in the state stream when composition ships. Given hard cutover (B9), a retroactive migration would require another quiet window.

### N6 — `ATTEMPT_BUDGET_CAP: u32 = 3` as a `const` in `orchestrator-types/src/lifecycle.rs`; `TeamTopology.max_retries` as soft cap capped at the const [REVISED, resolving C3]

My r1 did not stake a specific position on C3. Rust-systems N6 proposes the two-tier model: `const ATTEMPT_BUDGET_CAP: u32 = 3` (hard ceiling) + `TeamTopology.max_retries` (per-team soft cap where `max_retries <= ATTEMPT_BUDGET_CAP`). The daemon warns at dispatch time if `team.max_retries > ATTEMPT_BUDGET_CAP` and clamps to the const.

This does NOT violate CLAUDE.md §8b "no allowlist." The CLAUDE.md rule targets "metric allowlists" — ceilings that suppress quality scanner violations. A retry budget cap is a domain invariant (how many times should the substrate retry a brief before escalating to a human), not a quality metric. Per-topology configuration within a hard ceiling is a legitimate topology-author decision.

Resolution: adopt rust-systems' two-tier model. The `const` lives in `orchestrator-types/src/lifecycle.rs` alongside `BriefState`. The soft cap lives on `TeamTopology`. The FSM enforces `min(team.max_retries, ATTEMPT_BUDGET_CAP)`.

### N7 — No `ConnectionManager` in any lifecycle type signature [HOLD]

Unchanged from r1. The `const` approach to the budget cap actually strengthens this: the FSM transition function reads `attempt` from the variant payload (no Redis read) and compares to `ATTEMPT_BUDGET_CAP` (compile-time const, no config read). The entire budget check is purely computational.

### N8 — SETNX sentinel removed in the same PR as FSM landing [HOLD, NOW SHARED WITH SOLID]

Adopting solid's N7. Two dedup mechanisms for the same invariant ("one terminal verdict per brief") is a split-brain. The FSM terminal-state transition IS the dedup. The sentinel key `agentry:verdict:emitted:{brief_id}` and the `append_verdict_idempotent` SETNX pattern are removed in the FSM landing PR, not in a follow-up. This is a hard cutover (B9) requirement, not optional cleanup.

---

## §3 Still-contested-after-r2

### C-A — `Extension` variant dispatch: table-driven (solid) vs. enum match (rust-systems) [CONTESTED with solid]

**Solid's position (N4):** `BriefState::Extension(name, payload)` transitions must be resolved via a data-driven `HashMap<&str, ExtensionTransitionFn>`. Adding a new extension is one table insertion; no new match arms.

**Rust-systems' position (N2/N8):** `Extension(name: String, data: serde_json::Value)` is an open variant; exhaustive match MUST include an `Extension` arm. This is OCP-correct because adding a new extension does NOT require changing the match — the `Extension` arm is already present and the name-dispatch logic inside it can be extended by table.

**Clean-arch position:** I hold that the table-driven dispatch is over-engineering for the first slice (zero extension phases today; B6 is explicitly a follow-up). The OCP risk solid identifies is real, but the FENCE_MATRIX analogy does not hold cleanly here: FENCE_MATRIX is a pure data table (severity thresholds), while extension transitions have behavioral implications (different states can be reached from an extension phase). A behavioral dispatch table is closer to a strategy pattern than a configuration table.

The correct resolution for clean-arch is structural: `Extension` variant is defined from day one (variant exists, no dispatch table); the `handle` function returns `Err(InvalidTransition)` for any `Extension` state+event until the table is added. This is not OCP-safe at the match level, but it is honest — the transition table does not pretend to support extensions it cannot yet validate. When the first real extension phase ships (composition follow-up), the table is introduced in that PR alongside the first concrete extension. No table without a consumer.

**Contested with:** solid-architect.

### C-B — `BriefMetadata` as a separate Redis key vs. inline on `BriefStateRecord` [CONTESTED with solid]

**Solid's position:** `BriefMetadata` is a separate struct holding mutable audit fields (`attempt`, `reworks_used`, `agent_id`). This prevents variant payload inflation.

**Clean-arch counter:** `BriefMetadata` introduces a second Redis key per brief (`agentry:brief:{id}:metadata`?) that must be kept consistent with the state stream. Two keys for one logical record is a split-brain risk. If the daemon crashes between writing `BriefStateRecord` and updating `BriefMetadata`, the two are inconsistent. The 3-field cap is a useful discipline, but the mechanism to enforce it should be compile-time (adding a 4th field causes a review comment, not a type error). I prefer embedding `attempt: u32` in each non-terminal `BriefState` variant (it is the minimum context the FSM's pure `handle` needs) and keeping `BriefMetadata` as an in-memory struct that is not separately persisted — it is reconstructed from the state stream on replay by scanning for the highest `attempt` value.

If `BriefMetadata` is persisted as a separate Redis key, its writes must be atomic with the `BriefStateRecord` XADD — which requires expanding the Lua script from 2 operations (XADD + SET for state key) to 3 (XADD + SET state + SET metadata). This is operationally feasible but adds complexity for a benefit (cleaner variant payloads) that is primarily aesthetic.

**Contested with:** solid-architect. Rust-systems did not take a strong position on this; their `BriefStateRecord` includes `triggering_event: BriefEvent` without a separate metadata struct, implying they embed all necessary fields in the record.

### C-C — Replay source on daemon crash: trace stream or state stream [CONTESTED with rust-systems]

**Rust-systems' position (§2 `BriefStateStream`):** on daemon restart, the projector replays the **state log** from `0-0` to reconstruct current `BriefState`. The projected `state` key is authoritative when present; the log is recovery source when the key is absent.

**Clean-arch position (r1 N4):** the **trace stream** is the authoritative event log for replay. The state stream is a derived projection. The crash recovery path must replay the trace stream, not the state stream, to reconstruct FSM state.

**Why this matters:** if the daemon replays the state log for recovery, then the state stream becomes the authoritative source of truth — which means any event that drives an FSM transition MUST appear on the state stream, not only on the trace stream. Rust-systems' `BriefStateRecord` carries `triggering_event: BriefEvent`, which means the state stream implicitly encodes the events needed for replay. This IS sufficient for recovery — but it conflates the event log (trace stream, where raw agent events live) with the decision log (state stream, where FSM transitions live).

The practical distinction: if a trace event is emitted by an agent but the FSM projector crashes before translating it to a `BriefEvent` and writing the `BriefStateRecord`, replaying the state stream will miss that event. Replaying the trace stream will catch it. For the FSM to be genuinely event-sourced (derived from the trace), crash recovery must replay the trace stream and re-derive state.

**The pragmatic counter (rust-systems implicit):** state stream replay is faster (O(transitions) not O(all trace events)) and the `BriefStateRecord.triggering_event` gives enough information to recover. Agreed on performance. Contested on correctness: if the projector task is the translation boundary (trace → BriefEvent → FSM), and the projector crashes mid-translation, the state stream has the last confirmed state but not the in-flight event. The trace stream has both.

**Contested with:** rust-systems. This is the most architecturally load-bearing disagreement remaining.
