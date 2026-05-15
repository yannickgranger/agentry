# Clean-arch lens — r1

**Council:** lifecycle-state-machine-council
**Issue:** #246 — substrate brief lifecycle state machine
**Lens:** clean-arch
**Date:** 2026-05-02

---

## §1 Archaeology — clean-arch lens

### Current dependency direction

The two-crate split today is clean at the package level:

- `orchestrator-types` (`Cargo.toml`): zero runtime dependencies. Depends only on `serde`, `serde_json`, `chrono`, `uuid`, `thiserror`, `schemars`. No `redis`, no `tokio`, no `async-trait`. Instability = 0.0 — nothing inside `crates/` imports from it except `orchestrator-runtime`. This is the correct innermost ring.

- `orchestrator-runtime` (`Cargo.toml`): depends on `orchestrator-types` (path dep, line 22) plus `redis`, `tokio`, `async-trait`, `reqwest`, `ed25519-dalek`, `rusqlite`. Correct direction: outer depends on inner.

**Composition root:** `crates/orchestrator-runtime/src/bin/orchestratord.rs` (lines 1–20). The `main()` function calls `Config::load()?` and `daemon::run(&cfg).await`. No DI framework; all wiring is inside `daemon::run`. This is acceptably thin.

### Where Redis coupling actually lives today

The coupling is **not** at the binary boundary — it is distributed across `orchestrator-runtime` as a whole:

1. `daemon::run` opens a `ConnectionManager` directly at line 38 via `redis_io::connect`. This object flows from `run()` → `handle_brief()` → `redis_io::*` at every call site. There is no port trait between the FSM logic and Redis.

2. `handle_brief` (daemon.rs:223) accepts `conn: &mut ConnectionManager` as a direct parameter. All state reads/writes (`redis_io::fetch_team`, `redis_io::fetch_role`, `redis_io::append_verdict_idempotent`, `redis_io::append_trace`) are direct Redis calls — no trait abstraction layer. File: `crates/orchestrator-runtime/src/daemon.rs`, function signature at line 223.

3. The in-process FSM state today is a plain `HashMap<RoleRef, RoleState>` (daemon.rs:279) — allocated in `handle_brief`'s stack frame. It is purely in-memory, lost on crash, and there is no projection to any durable store.

4. Verdict emission uses `redis_io::append_verdict_idempotent` (redis_io.rs:98), which combines a SETNX sentinel and an XADD in one function — both are direct Redis commands with no port interface.

5. `projector::run` (projector.rs:34) also holds a `ConnectionManager` directly and runs a free-standing `XREAD BLOCK` loop. It is spawned as a `tokio::spawn` inside `daemon::run` (daemon.rs:59). No port trait here either.

### What the FSM inherits from today's design

Today's "FSM" is fully implicit:

- States: `RoleState { Pending, Running, Shipped, Failed }` at daemon.rs:203. These are role-level states within a single brief execution, not brief-level lifecycle states. They are not persisted anywhere.

- Transitions: the `'outer` loop body (daemon.rs:285–535) encodes transitions through direct conditional branches. No `handle(state, event) -> Result<NewState>` function exists yet.

- Events: consumed ad-hoc. `spawn_role` outcome is an in-memory `Verdict` struct (spawner.rs outcome processing path). No `BriefEvent` typed enum exists.

- Writer: the daemon is the sole writer of `active_briefs` (daemon.rs:96 SADD, daemon.rs:184 SREM) and of the SETNX sentinel. Multi-writer risk is theoretically present for sentinel races but handled by the `NX` flag.

### Composition root assessment

`orchestratord.rs` is two lines of logic: config load + daemon run. All wiring happens inside `daemon::run()` which is a library function, not a binary entry point. This means **DI wiring is buried in library code** (`orchestrator_runtime::daemon::run`), not in the binary. The binary cannot substitute adapters without changing library code. This is the key composition root violation the FSM proposal must address.

---

## §2 Proposed spec contribution — clean-arch lens

The following fragments follow the v3.3 graph-specs dialect. Each `##` heading maps to a Rust `pub` type; no convention headings without corresponding types.

### Fragment for `brief_lifecycle.md`

```markdown
# Brief lifecycle

The bounded context that owns the *explicit state machine governing a brief
from submission to terminal verdict*. Replaces the implicit `RoleState`
DAG-walker in `daemon::handle_brief` with a typed FSM whose transition
function is pure (no I/O, no async) and whose state is projected to a
durable append-only Redis stream.

The FSM has two physical homes: types live in `orchestrator-types` (zero
I/O dependencies); the driver loop that calls the FSM, reads events, and
projects state lives in `orchestrator-runtime`. This preserves the existing
instability gradient.

The composition root (`orchestratord` binary) wires the driver loop to the
`EventSource` and `StateProjector` ports before calling `daemon::run`. The
daemon library does not construct those adapters — it receives them.

## BriefState

The set of discrete lifecycle positions a brief occupies. Carries
structured payload at non-terminal states (agent_id, attempt counter,
rework target) so the FSM can enforce budget rules without external lookups.

Terminal states: `Shipped`, `Failed`, `ReworkedOut`, `Aborted`.
Non-terminal: `Submitted`, `Authoring`, `Verifying`, `Reviewing`,
`Shipping`, `Watching`, `Reworking`.

- depends on: BriefId
- depends on: ReworkTarget

## BriefEvent

Events that drive state transitions. All variants carry only domain-typed
data (BriefId, Ts, role names as strings, EventVerdict). No Redis types,
no ConnectionManager, no stream IDs in any variant signature.

- depends on: EventVerdict
- depends on: BriefId

## InvalidTransition

Returned when `brief_fsm::handle` is called with an event that is not
valid for the current state. Carries the source state name and event name
for diagnostics. Never silently dropped — the driver loop logs and
discards, but the FSM itself rejects loudly.

## ReworkTarget

Which upstream role a reviewer's `ReworkNeeded` verdict routes back to.
Extracted as a standalone type because both `BriefState::Reworking` and
`BriefEvent::ReviewerDone` reference it, and the composition must be
unambiguous.

## BriefStateRecord

One entry on the state stream. Carries `brief_id`, `state: BriefState`,
`transitioned_at: Ts`, `parent_brief_id: Option<BriefId>`,
`composition_role: Option<String>`. The `parent_brief_id` and
`composition_role` fields are `None` for standalone briefs and populated
for composition-member briefs per B3. This is the unit written by
`StateProjector` and read by operator dashboards.

- depends on: BriefState
- depends on: BriefId

## EventSource

Port trait. The daemon's FSM driver calls `next_event(&self, brief_id) ->
impl Future<Output = Result<BriefEvent, SourceError>>` to read the next
FSM-driving event off the trace stream. Implementors: `RedisEventSource`
(production), `VecEventSource` or `RecordedEventSource` (test doubles).

No `redis::Connection` or `ConnectionManager` appears in the trait
signature. The adapter decides how to map XREAD entries to `BriefEvent`
variants.

## StateProjector

Port trait. The FSM driver calls
`project(&self, record: &BriefStateRecord) -> impl Future<Output =
Result<(), ProjectorError>>` to durably persist a state transition.
Implementors: `RedisStateProjector` (production), `InMemoryStateProjector`
(test double).

No `ConnectionManager` in signature. The adapter decides how to map
`BriefStateRecord` to XADD + projected current-state SET.
```

### Fragment for `brief_state_stream.md`

```markdown
# Brief state stream

The bounded context that owns the *Redis stream contract for brief state
transitions*. Downstream of `brief_lifecycle` (which defines the types
written here) and upstream of the operator dashboard and future composition
reactive layer.

## StateStreamKey

The typed key formula for a brief's state log stream. Value:
`agentry:brief:{id}:state_log`. Distinct from the trace stream
(`agentry:brief:{id}:trace`): the trace stream carries all raw agent
events; the state stream carries only FSM transition records.

The projected current-state key (a plain Redis string, not a stream) is
`agentry:brief:{id}:state`. This is a convenience cache for O(1) current-
state reads; the stream remains the authoritative source for replay.

- depends on: BriefId

## StateStreamEntry

The XADD payload shape for one state transition on the stream. Fields:
`brief_id`, `state` (JSON-encoded `BriefState`), `transitioned_at` (ISO-
8601), `parent_brief_id` (optional), `composition_role` (optional),
`attempt` (u32 — current attempt counter at transition time).

Written exclusively by `RedisStateProjector`. Read by: operator dashboard,
daemon's crash-recovery replay path, future composition reactive layer.

- depends on: BriefStateRecord
```

### Fragment for `brief_retry_budget.md`

```markdown
# Brief retry budget

The bounded context that owns the *attempt counter and exhaustion rules for
a brief's retry lifecycle*. Separating this from `brief_lifecycle` lets the
cap value be a typed constant with a clear ownership story (no magic
numbers inside the FSM transition function).

## RetryBudget

Holds `max_attempts: u32` (default: 3, operator-overridable per brief) and
`current_attempt: u32`. Embedded in `BriefState::Authoring`, `Verifying`,
`Reviewing`, `Reworking` as a carry-forward field so the FSM transition
function can enforce the cap without a Redis read.

On `BriefEvent::RetryRequested`: FSM checks `budget.current_attempt <
budget.max_attempts`. If under: transition to `BriefState::Authoring` with
`attempt += 1`. If at cap: transition to `BriefState::Failed` with reason
`BudgetExhausted`. No separate `ReworkedOut` state needed for budget
exhaustion — `Failed { reason: BudgetExhausted }` is unambiguous.

- depends on: BriefState

## BudgetExhausted

Unit-like error type returned when `RetryBudget::increment()` is called
after `current_attempt == max_attempts`. Produced by the FSM transition
function; consumed by the driver loop to emit `BriefEvent::BudgetExhausted`
before writing the terminal `BriefState::Failed` record.
```

---

## §3 Non-negotiables — clean-arch lens

**N1 — FSM transition function is pure (no I/O, no async).**

`fn handle(state: &BriefState, event: &BriefEvent) -> Result<BriefState, InvalidTransition>` must compile without `async`, without `&mut ConnectionManager`, without any import from `redis` or `tokio`. The grill's B5 resolved this already; the non-negotiable is that the spec encodes it explicitly so the CI gate (via cfdb ban rule) can enforce it. Any PR that adds an async bound or a Redis import to the transition function is a gate violation.

**N2 — `EventSource` and `StateProjector` are ports, not inline Redis calls.**

Today's `handle_brief` calls `redis_io::*` directly (daemon.rs:309, 394, 559). The FSM driver must call these through port traits so the transition logic is testable without a live Redis instance. This is not optional — the six forensic cases (especially Case 3: stall after premature-shipped) cannot be reproduced in unit tests unless the event source is injectable. At least one test double (`VecEventSource`, `InMemoryStateProjector`) must be in `dev-dependencies`, not behind a feature flag, so the unit test suite can exercise the full transition table.

**N3 — Composition root is the binary, not the daemon library.**

`orchestratord.rs` must construct `RedisEventSource` and `RedisStateProjector` and pass them into the daemon's FSM driver. The daemon library function must accept them through trait bounds (or `Arc<dyn Trait>`). The current pattern (`daemon::run` constructs its own `ConnectionManager`) is the composition root violation that allowed the stall in Case 3 — the library could not be tested with an injected event source. This is **CONTESTED**: rust-systems may push back that `Arc<dyn Trait>` has runtime cost and prefer monomorphization. The architecture position is: the binary is the monomorphization site; the daemon library takes generics. `fn run<E: EventSource, P: StateProjector>(event_source: E, projector: P, cfg: &Config)`. No dynamic dispatch required.

**N4 — Trace stream is the authoritative event log; state stream is a derived projection.**

The crash recovery path (grill open item 4) must replay the trace stream, not the state stream, to reconstruct FSM state. This matters for hard cutover (B9): if a brief was mid-transition when the daemon crashed, the trace stream has the raw events; the state stream may have the last confirmed state but not the in-flight event. The FSM driver's replay function `rebuild_state(brief_id, event_source) -> BriefState` must be demonstrably derived from the trace stream alone. This also constrains `BriefEvent` variants: every `BriefEvent` that drives an FSM transition must have a corresponding `EventKind` variant on the trace stream. Events that exist only in the state stream (as transition records) cannot drive replay — they are projections, not inputs.

However, this is a **partial** event-sourcing posture, not pure event-sourcing. The state stream is a "command log" (FSM decisions persisted as first-class records), not a pure projection from the trace stream. The distinction matters for Case 1/3 root cause: the SETNX sentinel stored in the state of the Redis SET was the "single authoritative source" of brief terminal state, but it was set prematurely. The FSM replaces this with an explicit state record. The spec must be explicit that the state stream is "authoritative for current state" (after the FSM writes it) and the trace stream is "authoritative for replay from scratch". These are two different authorities and both must be preserved.

**N5 — `parent_brief_id` and `composition_role` on `BriefStateRecord` from day one, not in a follow-up.**

The grill's B3 resolved this. The non-negotiable is that the spec encodes `parent_brief_id: Option<BriefId>` as a first-class field on `BriefStateRecord`, not as a future extension variant. A `BriefStateRecord` without `parent_brief_id` would require a breaking migration when composition lands. Given hard cutover (B9), adding the field on day one costs nothing and saves one future drain cycle.

**N6 — Verdict stream derivation must flow through FSM terminal state, never bypass it.**

The grill's B7 resolved that `agentry:verdicts` stays and is derived from terminal-state transitions. The non-negotiable is that `append_verdict_idempotent` is called ONLY when the FSM writes a terminal `BriefState` (Shipped/Failed/ReworkedOut/Aborted). The SETNX sentinel (`agentry:verdict:emitted:{brief_id}`) is replaced by the FSM's terminal-state write being the single authoritative action. The verdict is a projection of the FSM terminal state, not a separate emission path. Any code path that calls `append_verdict_idempotent` without a preceding FSM terminal-state write is a port purity violation and reproduces the forensic cases 1/2/3. **NOT CONTESTED**: this is what the grill resolved in B7.

**N7 — No `redis::ConnectionManager` in any `BriefState`, `BriefEvent`, `InvalidTransition`, or `RetryBudget` type signature.**

These types belong in `orchestrator-types`. The Cargo.toml of `orchestrator-types` has no `redis` dependency. Any proposal to move these types to `orchestrator-runtime` (where Redis is available) violates the dependency rule: it makes the innermost ring depend on adapter-level infrastructure. **CONTESTED**: if rust-systems argues for a single-crate design where FSM types and Redis adapters coexist, the clean-arch position is that co-location breaks the ability to test the FSM in isolation and conflates the "what the FSM knows" (types crate) with "how the FSM persists state" (runtime crate).
