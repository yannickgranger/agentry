# rust-systems lens — r2

Council: `lifecycle-state-machine-council`
Lens: rust-systems
Date: 2026-05-02

---

## §1 Acknowledged from other lenses

### From solid-architect r1

**S-A1: `BriefMetadata` as the overflow type for variant payload.** Solid's N3
(max 3 fields per `BriefState` variant) is structurally correct. My r1 proposed
`Authoring { agent_id: String, started_at: Ts, attempt: u32 }` — 3 fields.
Solid's proposal to move `started_at` to `BriefStateRecord.at` (the record carries
the timestamp of the transition) is right: the variant need not carry `started_at`
separately when the envelope already has it. I withdraw `started_at` from the
`Authoring` variant payload. The `attempt: u32` counter belongs in `BriefMetadata`
(or `RetryBudget` per solid/clean consensus) rather than repeated across every
non-terminal variant.

My r1 had `attempt: u32` on every non-terminal variant (`Authoring`, `Verifying`,
`Reviewing`, `Shipping`, `Watching`, `Reworking`). Solid's `BriefMetadata` extracts
this. Accepted: `attempt` lives in `BriefMetadata` / `RetryBudget`, not scattered
across six variant payloads. The `BriefStateRecord` envelope carries the `attempt`
value at the time of the transition (as a snapshot field), so replayers can reconstruct
the counter from the log without loading `BriefMetadata` separately.

**S-A2: DOL-meta-lifecycle as a separate bounded context.** Solid's N5 (Case 6
workspace GC is a separate brief, not bundled into the FSM EPIC) is correct on CCP
grounds. My r1 did not address Case 6 directly. Accepted: the FSM emits the terminal
signal; the workspace module hooks on it. No GC logic inside the FSM module. This
also means `BriefStateRecord` does not need a `workspace_path` field.

**S-A3: `orchestrator-types/src/lifecycle.rs` module boundary.** Solid's N5 on
module boundary for the FSM types (`BriefState`, `BriefEvent`, etc.) is correct.
They should not be merged into `brief.rs` or `verdict.rs`. A dedicated
`orchestrator-types/src/lifecycle.rs` (with `pub use` in `lib.rs`) is the right
first step. Crate split deferred until a second consumer (dashboard) materialises.
I did not explicitly propose the module boundary in r1 — accepting now.

**S-A4: SETNX sentinel removal in the SAME PR as FSM landing.** Solid's N7. I
implicitly assumed this in r1 but did not state it. Accepting explicitly: the SETNX
sentinel at `agentry:verdict:emitted:{brief_id}` (redis_io.rs:102) and the
`append_verdict_idempotent` function's NX gate logic must be removed in the same
PR that lands the FSM. Two dedup mechanisms for the same invariant is a split-brain.

### From clean-arch r1

**C-A1: Generic bounds at the library function signature.** Clean's N3 proposes
`fn run<E: EventSource, P: StateProjector>(event_source: E, projector: P, cfg: &Config)`
with monomorphization at the binary's composition root. This is the correct resolution
to C1 (see §2 below) and I accept it fully. The Rust compiler monomorphizes at the
binary; no `Arc<dyn>` overhead; test doubles are injected at the call site. The binary
(`orchestratord.rs`) is the monomorphization site.

**C-A2: `ReworkTarget` as a standalone pub type.** Clean's fragment for `brief_lifecycle.md`
extracts `ReworkTarget` as a pub type with its own spec heading. This is correct per
the dialect rule (every `##` heading must map to a Rust pub type). My r1 used
`ReworkTarget` in the `Reworking` variant without giving it a spec heading. Accepting:
`ReworkTarget` gets its own `##` heading and its own pub type in `lifecycle.rs`.

**C-A3: Trace stream is authoritative for replay; state stream is derived.** Clean's N4
makes this distinction explicit. My r1 said "replay from `0-0`" on the state log but
was imprecise about WHICH stream is replayed. Accepting clean's framing: trace stream
is the authoritative input log; state stream is a derived projection. Crash recovery
replays trace events through the `handle` function to reconstruct `BriefState`.
The `BriefEvent` vocabulary must therefore have a 1-to-1 mapping to trace-stream
events — no `BriefEvent` variant can exist that has no corresponding `EventKind`
that can appear on the trace stream. `RetryRequested` and `AbortRequested` need to
be added to the trace stream (emitted by the daemon's operator-command handler) so
they are replayable.

**C-A4: `BudgetExhausted` as a unit type, not just a `BriefEvent` variant.** Clean's
proposal separates `BudgetExhausted` as an error type returned by `RetryBudget::increment()`.
This is cleaner than having `BudgetExhausted` as both a `BriefEvent` variant and an
internal function call. Accepted: `BudgetExhausted` is a distinct pub type; the FSM
transition function produces it when the attempt counter hits the cap; the driver loop
emits it to the trace stream so the terminal transition is replayable.

---

## §2 Revised non-negotiables (r2 positions on N1-N8 + C1-C3)

### N1 (held): `BriefState`, `BriefEvent`, `InvalidTransition` in `orchestrator-types/src/lifecycle.rs`

Unchanged from r1. Sole addition from S-A3: `lifecycle.rs` is a dedicated module,
not merged into existing modules. The instability of `orchestrator-types` remains 0.0.

### N2 (held): No FSM library (`statig`). Hand-rolled enum + pure transition function.

No change to the substance. Strengthened rationale against the `statig` question:
even when composition ships, the hierarchy is brief-parent / brief-child relationships
across independent flat FSMs — not HSM sub-states within a single machine. `statig`
would import a library whose type-system model (`impl statig::State for MyState`)
structurally inverts the approach: it forces state behaviour into associated types on
the enum rather than into a single `handle` function. The pure-function approach
tested by the council (per B5) remains correct and is not at risk of a rewrite.

### N3 (STRENGTHENED): Lua script is required. Atomic `state_log XADD + state SET`.

Clean's N4 reinforces this: the state stream is a derived projection, not the
authoritative source. The projected `state` key is a performance cache. If the
atomic Lua write fails (Redis Lua OOM abort), the transition is not persisted —
but the trace stream still has the source event. Replay reconstructs state
correctly. The Lua path is therefore required for cache coherence, not for
correctness. The correctness guarantee comes from the trace stream being the
authoritative log. The non-negotiable stands: XADD to `state_log` and SET to `state`
key must be in one Lua script. If Lua is explicitly disabled on the Redis instance,
the daemon must refuse to start — not fall back silently to non-atomic writes.

### N4 (held): `BriefEvent` is distinct from `EventKind`.

Clean's N2 and solid's N2 both agree. No change to the substance. Added from C-A3:
every `BriefEvent` variant must have a corresponding trace-stream event that the
projector constructs it from. This means `RetryRequested` and `AbortRequested` must
be emitted to the trace stream (as `EventKind::Event { payload: { "type": "retry_requested", ... } }`)
by the operator-command path before being translated to `BriefEvent`. The `handle`
function never sees `EventKind` — only `BriefEvent`.

### N5 (REVISED): No fixed TTL on `state` key or `state_log` stream.

Identical substance to r1. Revised placement: this is an operational invariant for
the `BriefStateStream` spec section, not the `BriefState` spec section.

### N6 (REVISED): `ATTEMPT_BUDGET_CAP` — moved to `pub const DEFAULT_ATTEMPT_CAP: u32 = 3`

My r1 proposed a hard-ceiling `ATTEMPT_BUDGET_CAP` const that overrides topology
config. After reading solid's `RetryBudget` proposal and clean's `RetryBudget`
(with `max_attempts: u32` operator-overridable per brief), I revise:

**The const is the DEFAULT, not a hard ceiling enforced against topology config.**
The name changes to `DEFAULT_ATTEMPT_CAP: u32 = 3` (matching solid's naming).

The concern that operators could set `max_retries = 100` in a topology and grind
forever is valid. The mechanism to prevent this is NOT a silent override of the
topology's setting — that would be a hidden coupling. The correct mechanism is a
**validation check at dispatch time**: when the daemon reads the topology's
`max_retries` field, it validates `max_retries <= MAXIMUM_ATTEMPT_CAP` (a separate
const, e.g. `pub const MAXIMUM_ATTEMPT_CAP: u32 = 10`) and refuses to dispatch if
it exceeds this. The validation failure is logged and the brief is moved to
`BriefState::Aborted` with `reason: "topology max_retries exceeds ceiling"`. This
makes the ceiling explicit and operator-visible (the brief aborts with a clear reason)
rather than silently capping.

Two constants: `DEFAULT_ATTEMPT_CAP: u32 = 3` (the default when no topology config
is provided) and `MAXIMUM_ATTEMPT_CAP: u32 = 10` (a hard validation ceiling at
dispatch time). Both are `pub const` in `orchestrator-types/src/lifecycle.rs`.
Both are editable only via reviewed PR per CLAUDE.md §8.

**CONTESTED with clean-arch** on whether `MAXIMUM_ATTEMPT_CAP` should exist at all
vs. a pure-topology-config model. My position: a runtime-configurable cap that can
be set arbitrarily is a latent grinding risk. A compile-time ceiling that validation
enforces gives operators flexibility (1–10) while preventing pathological configs.

### N7 (held): No `#[non_exhaustive]` on `BriefState` or `BriefEvent`.

The compiler's exhaustiveness check on `match` arms IS the safety net for extension
variants. Applying `#[non_exhaustive]` suppresses it. Every match site must handle
`Extension(name, data)` explicitly when the variant ships. That compile error is the
OCP enforcement mechanism, not an obstacle.

One revision from solid's N4 on extension dispatch: I now agree that a match arm
`Extension(name, data) => dispatch_extension(name, data, &EXTENSION_TABLE)` is
the correct shape. The `EXTENSION_TABLE` is a static slice (same pattern as
`FENCE_MATRIX`) so adding a new topology-specific phase is one row, not a new
match arm. The `dispatch_extension` function takes a `&[ExtensionTransitionRow]`
parameter so the table is injectable in tests without changing the function. This
resolves C2.

### N8 (STRENGTHENED): Two concurrent async loops per brief — explicit sequencing invariant.

My r1 surfaced this but did not articulate the race. Expanded here for synthesis.

**The race condition:**

The daemon today has one Tokio task per brief: the `handle_brief` task, which both
drives role spawning and observes outcomes. Under the FSM, the daemon needs two tasks:

1. The **orchestrator task** (evolved `handle_brief`): drives role fan-out, calls
   `spawner::run_agent`, receives `Verdict` outcomes, emits `EventKind::Done` to the
   trace stream.

2. The **FSM projector task** (new): subscribes to `agentry:brief:{id}:trace` via
   `XREAD BLOCK`, maps `EventKind::Done + role_name` to `BriefEvent`, calls `handle`,
   writes `BriefStateRecord` to `state_log` and `state` key.

**The race:** the projector task subscribes to the trace stream. It uses `XREAD BLOCK`
starting from `$` (last delivered) or a stored offset. If the projector task starts
AFTER the orchestrator task has already emitted trace events, those events are missed.
The projector never sees the first `CoderStarted` or even `CoderDone` event if there
is any scheduling gap between the two `tokio::spawn` calls.

**The minimum mechanism that prevents it:**

The projector must start from offset `0-0` (replay from the beginning of the brief's
trace stream), not from `$` (new-only). Since the brief has just been submitted, the
trace stream is empty at dispatch time — replaying from `0-0` means the projector
catches all events from the start, regardless of scheduling order between the two tasks.

Concretely:
```
dispatch order:
1. orchestratord spawns orchestrator task for brief B
2. orchestratord spawns projector task for brief B (starts XREAD from "0-0")
3. orchestrator task fires first role → emits EventKind::Done to trace
4. projector task's XREAD from "0-0" picks up entry from step 3
```

If the projector task starts first (step 2 before step 3), `XREAD BLOCK` from `0-0`
blocks until the orchestrator emits the first event. No missed events.

If the orchestrator task runs first (step 3 before step 2), the projector's `XREAD`
from `0-0` on startup picks up the already-written entry on its first read. No missed events.

**`$` (new-only) is wrong for the projector.** It would miss all events emitted before
the projector's first `XREAD` call. This is the concrete failure mode: if the coder
role finishes in < 1ms (unlikely in production but possible in tests with
`VecEventSource`), the `CoderDone` event is on the stream before the projector task
even starts. Projector reads from `$`, sees nothing, and the brief stalls at
`Submitted` state forever.

**Operational invariant for the synthesis:**

> The FSM projector task for brief B MUST start its first `XREAD` from stream offset
> `0-0`, not from `$`. The projector stores its last-consumed offset in Redis at
> `agentry:brief:{id}:state_projector_cursor` (a STRING key) and resumes from that
> offset on daemon restart. On first start (key absent), offset is `0-0`. This
> eliminates the startup race without requiring a consumer group, while preserving
> event-replay on crash recovery.

This invariant is distinct from the Lua atomicity requirement (N3). N3 prevents
cache incoherence between `state_log` and `state` key. This invariant prevents
missed trace events at projector startup.

---

## §3 Still-contested-after-r2

### SC1 — `Arc<dyn EventSource>` vs. generic bounds — RESOLVED in favor of generic bounds

This is not contested after r2. Clean's N3 proposes generics; I accept. Binary is the
monomorphization site. No `Arc<dyn>`. No runtime dispatch overhead. No cost.

Resolution: `fn run<E: EventSource, P: StateProjector>(event_source: E, projector: P, cfg: &Config)`.

### SC2 — `MAXIMUM_ATTEMPT_CAP` as a hard validation ceiling — CONTESTED with clean-arch

**My position (rust-systems):** two constants — `DEFAULT_ATTEMPT_CAP: u32 = 3` and
`MAXIMUM_ATTEMPT_CAP: u32 = 10`. Dispatch-time validation enforces the ceiling. Brief
aborts with clear reason if topology config exceeds the ceiling.

**Clean-arch likely position:** pure topology config — the cap is fully determined by
`RetryBudget.max_attempts: u32` set by the operator. No compile-time ceiling beyond
the type's `u32::MAX`. Trust operators to set reasonable values.

**Why I hold:** the forensic record shows X.0 grinding to v5 before the operator
noticed. A pure topology-config model means the ceiling is only as reliable as the
operator's attention. A compile-time `MAXIMUM_ATTEMPT_CAP` makes "this config is
pathological" a dispatch-time error, not a silent grinding loop. The 10x headroom
(default 3, ceiling 10) preserves operator override for known-hard problems while
capping at a reasonable bound. Per CLAUDE.md §8: thresholds are `const` values, not
runtime configs. Extending this principle: the CEILING on topology-configurable
thresholds is also a `const`.

### SC3 — `ReworkedOut` as a distinct terminal state vs. `Failed { reason: BudgetExhausted }` — CONTESTED with clean-arch

**My position (rust-systems) and solid-architect's position:** `ReworkedOut` is a
distinct terminal state in the `BriefState` enum. Rationale: the operator's response
to `ReworkedOut` (budget exhaustion, needs manual retry with override) is different
from `Failed` (infrastructure error or explicit coder failure). A single `Failed`
variant with an opaque `reason` string forces the dashboard and operator tooling to
pattern-match on reason strings to distinguish budget exhaustion from infrastructure
failure — that is a split-brain at the type level. Typed terminals are the Rust-idiomatic
way to express distinct operator-visible outcomes.

**Clean-arch's position (from r1):** `Failed { reason: BudgetExhausted }` — no
separate `ReworkedOut` state. One `Failed` terminal covers all failure modes.

**Why I hold:** dashboard queries and operator escalation paths need to distinguish
"brief ran out of retries (retry with override flag)" from "brief failed due to infra
error (diagnose infra)". If these both land in `Failed`, the dashboard must inspect
the `reason` string. String inspection is fragile across versions. Typed terminals
are exhaustively matchable and version-safe.

Counter: solid's r1 and my r1 both propose `ReworkedOut` as a named terminal.
If clean-arch holds on `Failed { reason: BudgetExhausted }`, this is a 2-vs-1
contested item for synthesis to adjudicate.

### SC4 — `BriefMetadata` as a separate type vs. carried in `BriefStateRecord` — NEEDS synthesis

Solid proposes `BriefMetadata` as a Redis-stored mutable state type alongside the
FSM. Clean embeds `RetryBudget` inside `BriefState` variants (carried forward on each
transition). My r1 had `attempt` on every variant — which I accepted should move to
a dedicated type in §1. The open question: does `BriefMetadata` exist as a separate
Redis key, or is the attempt counter fully reconstructible from the `BriefStateRecord`
log (each record carries `attempt: u32` as a snapshot field)?

**My position (rust-systems):** the attempt counter should be carried in `BriefStateRecord`
as a snapshot field (clean reconstruction from log replay, no separate key) AND in the
current `BriefState` variant payload as a `RetryBudget` embedded struct. This is clean's
approach. Solid's `BriefMetadata` as a separate Redis-stored type is an additional write
surface that can diverge from the state log. The Lua script already writes the state log
and the projected `state` key — adding a third key (`BriefMetadata`) to the same Lua
script is workable but adds complexity. Prefer: `RetryBudget` embedded in non-terminal
`BriefState` variants (carried forward through transitions), snapshotted in each
`BriefStateRecord`. No separate `BriefMetadata` Redis key.

This is the clean-arch position. It is not yet confirmed by clean-arch's r2.
