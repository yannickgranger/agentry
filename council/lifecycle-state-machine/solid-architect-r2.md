# solid-architect lens — r2

**Council:** lifecycle-state-machine
**Issue:** #246 — substrate brief lifecycle state machine
**Lens author:** solid-architect
**Date:** 2026-05-02

---

## §1 Acknowledged from other lenses

### From rust-systems r1

**Accepted — role-aware projection at the translation boundary.**
rust-systems r1 §1.8 makes a point I underspecified in r1: `BriefEvent` variants are constructed by the projector, not by the agent. `CoderDone` vs `ReviewerDone` is determined by role name at the translation boundary, not by the event variant. My r1 said "the daemon translates from `EventKind` to `BriefEvent`" without grounding this in how the role context is available. Accepted: the projector reads the trace entry's `agent_id` field and maps (role_name, EventKind::Done) → BriefEvent variant. This makes the translation boundary a two-input function, not a one-input cast.

**Accepted — `reworks_used` in process memory is not persisted across restart (§1.6).**
rust-systems r1 §1.6 cites `daemon.rs:277` explicitly: `reworks_used: u32` is per-execution, in process memory, not persisted. A daemon crash mid-rework-cycle resets the counter to 0, allowing the rework budget to be silently bypassed across restarts. My r1 called for FSM state persistence but did not name this specific exploit. The FSM fix is load-bearing for correctness, not just observability. Accepted into my §3 non-negotiables framing.

**Accepted — three verdict emission sites, not two.**
rust-systems r1 §1.2 identifies three `append_verdict_idempotent` call sites: role-outcome path (daemon.rs:394), handler-error path (daemon.rs:157), DOL composition path (compose_meta_verdict). My r1 said "inline verdict emission" without distinguishing them. The FSM eliminates path (1); path (2) becomes a synthetic abort event; path (3) is unaffected in this slice. Accepted: the spec must address all three paths explicitly, not just the obvious one.

**Accepted — `ATTEMPT_BUDGET_CAP` as hard ceiling with `TeamTopology.max_retries` as soft cap.**
rust-systems N6 proposes: `ATTEMPT_BUDGET_CAP: u32 = 3` is the hard ceiling const; `TeamTopology.max_retries` can be less than or equal to it as a per-team soft cap; the daemon warns at dispatch time when team config exceeds the hard ceiling. This resolves the tension between operator-configurable retry depth and compile-time safety. I had deferred the cap question in r1. Accepted as my position.

**Accepted — `BriefStateProjector` as concrete struct in `orchestrator-runtime`, not a port trait.**
rust-systems §2 `brief_state_stream.md` proposes `BriefStateProjector` as a concrete async struct — the pure `handle` function is the testable unit, and the projector is the async shell. This aligns with the existing `append_verdict_idempotent` pattern (pure function in types, async wrapper in redis_io). Accepted.

**Accepted — `#[non_exhaustive]` is wrong for `BriefState` and `BriefEvent` (N8).**
rust-systems N8: do NOT add `#[non_exhaustive]` — the compiler's exhaustiveness check at every match site is the safety net. Adding a new `Extension` variant later SHOULD produce compile errors at every match site. My r1 did not address this. Accepted: `BriefState` and `BriefEvent` must NOT be `#[non_exhaustive]`.

### From clean-arch r1

**Accepted — composition root violation is load-bearing for testability (N3).**
clean-arch r1 §1 identifies: `daemon::run` constructs its own `ConnectionManager` directly (daemon.rs:38). DI wiring is buried in library code, not the binary. This means the FSM driver cannot be tested with injected event sources without changing library code. This is the structural root cause of the "cannot reproduce Case 3 stall in unit tests" problem. Accepted: the composition root must move to the binary.

**Accepted — `ReworkTarget` as a standalone named type.**
clean-arch r1 §2 proposes `ReworkTarget` as an extracted type because both `BriefState::Reworking` and `BriefEvent::ReviewerDone` reference it. My r1 omitted this extraction. Accepted: shared structural data referenced by both the state type and the event type must be named, not inlined as string/u32 in two places.

**Accepted — `StateStreamEntry` vs `BriefStateRecord` distinction.**
clean-arch r1 §2 proposes `StateStreamEntry` as the XADD payload shape (wire format) distinct from `BriefStateRecord` (domain record). This is the same ISP reasoning I applied to `BriefEvent` vs `EventKind`: the wire format and the domain type should not be the same struct, because the wire format can change (field order, key names, added fields) without the domain type needing to change. However, I note that rust-systems uses `BriefStateRecord` as both the domain record and the serialized payload. If these collapse into one struct with serde derives, the distinction is thin. I will hold this as a mild preference (named separately) rather than a non-negotiable.

**Accepted — trace stream is authoritative for replay; state stream is derived projection (N4).**
clean-arch N4 articulates the two-authority model clearly: trace stream = authoritative for replay from scratch; state stream = authoritative for current state (after FSM writes it). These are different authorities. This is more precise than my r1 framing. Accepted: the spec must explicitly name both authorities and their respective scope.

---

## §2 Revised non-negotiables

### N1 — Pure transition function in `orchestrator-types` (HELD, clarified)

`fn handle(state: &BriefState, event: &BriefEvent) -> Result<BriefState, InvalidTransition>` lives in `orchestrator-types`. Zero I/O, zero async, zero redis import. This is SDP: `orchestrator-types` has instability I=0.0 (no crates/ imports from it except orchestrator-runtime). The daemon calls it; nothing stable depends on the runtime. Pattern precedent: `VerdictKind` and `compose_verdict_parts` (daemon.rs:876) demonstrate the split — pure compose logic is already extractable from the async layer.

No change from r1. Both rust-systems N1 and clean-arch N1 agree.

### N2 — `BriefEvent` is narrow, not a re-export of `EventKind` (HELD, grounded)

`BriefEvent` must be defined independently of `EventKind`. Two lenses confirm this (rust-systems N4, clean-arch implicit in EventSource port design). Additional grounding from r1 reading: the translation is role-aware — the projector needs (role_name + EventKind::Done) to construct the correct `BriefEvent` variant. If `BriefEvent` were a subset/alias of `EventKind`, the role-context mapping would be unrepresentable in the type system.

### N3 — Anti-god-state: BriefState variant payload cap of 3 fields (REVISED DOWN — conditional)

My r1 set this as an absolute cap. After reading rust-systems §2, I revise: the cap is a code review heuristic, not a spec rule. rust-systems' proposed state set has `attempt: u32` on every non-terminal variant (5–6 variants carrying attempt). This is not god-state — it is a load-bearing carry-forward that allows the FSM transition function to enforce the budget cap without an external lookup. The correct formulation:

A `BriefState` variant carries a field if and only if the transition function NEEDS that field to compute the next state. Fields that are only needed by external consumers (e.g., `started_at: Ts` for dashboard display) belong in `BriefStateRecord`, not on the variant. `agent_id` is borderline — the transition function may not need it, but `BriefStateRecord` does for lineage. My revised position: `agent_id` goes in `BriefStateRecord`, not on the `Authoring` variant. `attempt` stays on all non-terminal variants because the transition function computes the next state from it.

### N4 — Table-driven Extension dispatch (REVISED — weakened)

My r1 proposed `const EXTENSION_TABLE` per FENCE_MATRIX precedent. After the C2 question and reading both lenses, I revise.

FENCE_MATRIX worked because the matrix IS the policy data — it is statically knowable at compile time and is the right representation for that domain. For topology-specific FSM phases, the "policy data" is the per-extension transition rules, which are NOT statically knowable at `orchestrator-types` compile time (they depend on which topologies exist, which is runtime-registered).

Revised position: the `Extension(name: String, data: serde_json::Value)` variant is correct (rust-systems N2 reasoning: open variant, not an HSM sub-state). The OCP requirement is met by the open variant itself — adding a topology-specific phase does NOT require modifying `BriefState` or `handle`. The extension dispatch in `handle` takes the form:

```
BriefState::Extension(name, _) | BriefEvent::ExtensionEvent { phase, .. } =>
    extension_registry.dispatch(name, state, event)
```

where `extension_registry` is an argument to `handle` (or a thread-local in the runtime layer). This is OCP-correct: new extensions are registrations, not new match arms.

However, I withdraw the "must be a static const table" requirement. The registry form is sufficient for OCP. Whether it's `HashMap<&str, fn>` or `Vec<(name, handler)>` is an implementation choice the rust-systems lens should decide.

**Withdraw the FENCE_MATRIX analogy.** It was a false precedent — FENCE_MATRIX is a policy table, not an extension registry.

### N5 — Composition root in the binary (HELD, upgraded to non-negotiable)

My r1 mentioned this as a recommended boundary; after clean-arch N3 I upgrade it to non-negotiable. `orchestratord.rs` must construct `RedisEventSource` and `RedisStateProjector` and inject them. The daemon library function receives them as generic or trait-object parameters. The current pattern (daemon.rs:38: `ConnectionManager` constructed inside `run()`) is the composition root violation that makes Case 3 unreproducible in tests.

On the C1 monomorphization vs `Arc<dyn>` question: **monomorphization is correct**. `fn run<E: EventSource, P: StateProjector>(event_source: E, projector: P, cfg: &Config)`. No dynamic dispatch needed. The binary is the monomorphization site — it passes `RedisEventSource` and `RedisStateProjector`. Test code passes `VecEventSource` and `InMemoryStateProjector`. Zero runtime cost. This does NOT change the SRP/CCP read: the daemon with generic parameters is still the same CCP group (Brief-lifecycle state management). The cohesion analysis is the same whether the ports are generic or dyn.

### N6 — Case 6 (stale-worktree GC) is a separate brief (HELD)

No lens contested this. Held as stated in r1.

### N7 — SETNX sentinel removed in same PR as FSM (HELD)

No lens contested this. Held as stated in r1.

### N8 — `attempt: u32` persisted in FSM state, not in process memory (NEW — from rust-systems §1.6)

The FSM state record must carry `attempt: u32` so that a daemon crash during a rework cycle does NOT reset the counter to zero. `reworks_used: u32` in `handle_brief`'s stack frame (daemon.rs:277) is the exact gap that allows budget exhaustion to be silently bypassed across restarts. This is a correctness requirement, not a telemetry requirement.

### N9 — `ATTEMPT_BUDGET_CAP` is a hard ceiling const; `TeamTopology.max_retries` is a soft per-team cap (NEW — from rust-systems N6)

`pub const ATTEMPT_BUDGET_CAP: u32 = 3` in `orchestrator-types`. `TeamTopology.max_retries` can be at most `ATTEMPT_BUDGET_CAP`. If team config exceeds the const, the daemon logs a warning at dispatch time and enforces the const. The cap value of 3: today's X.0 run needed 5 outer attempts. Cap=3 with an operator override flag (rust-systems' `override_budget: bool` field on `RetryRequested`) is the correct balance. The default must be conservative; the override is explicit and auditable.

---

## §3 Still-contested after r2

### C-A — Two concurrent async loops per brief and CCP read (vs rust-systems)

rust-systems §1.4 surfaces a sequencing concern I did not name in r1: under B4, the daemon has two concurrent async loops per brief — the orchestrator task (drives role fan-out, current `handle_brief`) and the FSM projector task (reads trace events, projects state transitions). These are not simple re-uses of the same task — they are concurrent reads of the same trace stream from different consumers with different purposes.

My r1 said "adding the FSM consumer is CCP-correct because it replaces the scattered Brief-lifecycle state group." I maintain this CCP read, but I now qualify it: the two-loop architecture means the daemon's `handle_brief` task and the FSM projector task must not duplicate each other's decisions. Specifically:

- `handle_brief` today makes role-scheduling decisions by examining verdicts in process memory.
- The FSM projector makes lifecycle state decisions by reading the trace stream.

If `handle_brief` continues to make scheduling decisions independently of the FSM (e.g., it still checks `RoleState::Shipped` in its local HashMap), then there are TWO authorities for "what has this role done?" — the in-process HashMap and the FSM state. That IS a split-brain at the CCP level.

**My contested position:** the FSM projector must become the SOLE authority for lifecycle state. `handle_brief`'s `HashMap<RoleRef, RoleState>` must be replaced by reads from the FSM projector (either via projected state key or via an in-process channel from the projector). If the HashMap persists as a parallel authority, the two-loop architecture reproduces the same split-brain the FSM was supposed to fix — just at a lower level.

**Who contests this:** rust-systems §1.4 describes the two-loop architecture neutrally without resolving which is authoritative. clean-arch does not address this directly. This is an open question for r3 or captain resolution.

### C-B — `RetryBudget` as a standalone type vs embedded fields (vs clean-arch)

clean-arch §2 proposes `RetryBudget` as an embedded struct in `BriefState` non-terminal variants, carrying `max_attempts: u32` and `current_attempt: u32`. My r1 proposed `BriefMetadata` as a separate type for mutable audit fields. rust-systems proposes `attempt: u32` directly on each non-terminal variant with the cap enforced by `ATTEMPT_BUDGET_CAP` const.

I remain mildly at odds with clean-arch's `RetryBudget` embedding. The reason: `max_attempts: u32` on `RetryBudget` means the per-brief max is carried inside the FSM state record. This creates two sources for the max: the const `ATTEMPT_BUDGET_CAP` and the per-record `max_attempts`. If they diverge (operator sets max_attempts=5, const is 3), the FSM must have a rule for which wins. This is the same problem as having `TeamTopology.max_retries` AND `ATTEMPT_BUDGET_CAP` — and the resolution is the same: the const wins, and carrying `max_attempts` in the record is redundant if the const is always authoritative.

**My position:** `attempt: u32` on non-terminal variants (rust-systems shape), with `ATTEMPT_BUDGET_CAP` as the sole cap. No embedded `RetryBudget` struct. `BudgetExhausted` as a terminal reason variant on `BriefState::Failed` (clean-arch has this right), not a separate terminal state `ReworkedOut` (which is rust-systems' shape — contested).

On `ReworkedOut` vs `Failed { reason: BudgetExhausted }`: clean-arch's position (just `Failed`) is architecturally cleaner — one terminal for "brief did not ship" with a reason field. `ReworkedOut` as a distinct terminal implies operators need to distinguish "budget exhausted terminal" from "other Failed terminal" in their tooling. That is a SRP concern for the operator-facing surface, not for the FSM itself. I lean toward clean-arch's shape here.

**Contested with:** rust-systems (over `ReworkedOut` as distinct terminal). This warrants a captain ruling.

### C-C — Consumer group vs plain XREAD for FSM projector (vs rust-systems N7)

rust-systems N7 (CONTESTED) argues for consumer group (`XGROUP CREATE` + `XREADGROUP`) for the FSM projector, providing PEL and delivery guarantees across daemon restarts. The counter (also in rust-systems N7): if the projector replays from `0-0` on restart, the PEL provides no benefit and adds operational complexity.

My position on this: this is an implementation concern, not a SOLID concern. My lens does not have a strong position on XREADGROUP vs XREAD BLOCK with persisted offset. However, I note that the **persisted offset** is non-negotiable from an SRP standpoint — without it, the projector replays the ENTIRE trace stream on every restart for every in-flight brief, which for long-running briefs (X.0 v5 ran for a full drain session) is prohibitive. Whether the offset is stored in a Redis key or in the PEL is an implementation choice. I leave this to rust-systems to resolve with clean-arch.

