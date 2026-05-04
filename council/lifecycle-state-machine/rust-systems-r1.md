# rust-systems lens — r1

Council: `lifecycle-state-machine-council`
Lens: rust-systems
Date: 2026-05-02

---

## §1 Archaeology — rust-systems lens

### 1.1 Existing type inventory in orchestrator-types

**`EventVerdict`** — `crates/orchestrator-types/src/event.rs:21`
Five variants: `Shipped, Failed, Escalated, ReworkNeeded, Rejected`.
This is the *per-role, per-agent* wire type emitted on stdout NDJSON.
It travels on `agentry:brief:{id}:trace` embedded in `EventKind::Done`.

**`EventKind`** — `crates/orchestrator-types/src/event.rs:58`
Seven variants including `Done { verdict: EventVerdict, reason: Option<DoneReason> }`.
The `Done` variant IS the only terminal signal on the trace stream today.
The proposed `BriefEvent::CoderDone(EventVerdict)` wraps this type — no collision,
but there is semantic overlap: both carry `EventVerdict`. The FSM's `BriefEvent`
wraps the agent-level `EventVerdict` and adds the role context (which role produced
the verdict). This is a deliberate mapping layer, not a duplication.

**`VerdictKind`** — `crates/orchestrator-types/src/verdict.rs:19`
Eight variants: `Shipped, Failed, Escalated, PermitViolation, BudgetExceeded, Aborted, Rejected, ReworkNeeded { findings: Vec<ReviewFinding> }`.
This is the *team-level, brief-level* terminal record shape. It is richer than
`EventVerdict` (adds `PermitViolation`, `BudgetExceeded`, `Aborted`; `ReworkNeeded`
carries findings here, whereas `EventVerdict::ReworkNeeded` is finding-less).

**`VerdictKind::from(EventVerdict)`** — `crates/orchestrator-types/src/verdict.rs:30`
Conversion exists. The FSM's terminal state maps to `VerdictKind`, not `EventVerdict`.
This chain is: `EventVerdict` (agent wire) → `BriefEvent` (FSM input) → `BriefState`
terminal → `VerdictKind` (brief record). The From impl is already the bridge between
layers 1 and 4 — the FSM adds layers 2 and 3 in the middle.

**`Verdict`** — `crates/orchestrator-types/src/verdict.rs:46`
Struct with `brief: BriefId, kind: VerdictKind, at: Ts, trace_stream: String, reason: Option<String>`.
The FSM does NOT replace `Verdict` — it triggers `Verdict` creation at terminal
state transitions only.

**`Brief`** — `crates/orchestrator-types/src/brief.rs:73`
Has `parent_brief: Option<BriefId>`. No `attempt: u32`, no `composition_role`.
B3 and B8 require adding `attempt: u32`, `parent_brief_id: Option<BriefId>` (already
exists as `parent_brief`), and `composition_role: Option<CompositionRole>` (new field)
to the state record — but NOT to `Brief` itself. The `Brief` is the dispatch
payload; the attempt counter belongs on the FSM state, not on the immutable Brief.
Conflating the two would violate the daemon-only-writer rule (B4) because the Brief
is submitted immutably to the stream.

### 1.2 Daemon's current verdict-emission paths

There are **three emission sites** for `append_verdict_idempotent` in daemon.rs:

1. **Role-outcome path** — `daemon.rs:394`: called once per role in the outcome
   processing pass within `handle_brief`. This is the primary source of premature-shipped
   verdicts (Cases 1–3 in the forensic snapshot). Every role that completes triggers
   a verdict XADD attempt. The SETNX gate stops the second one — but the *first* one
   fires at role completion, not at brief terminal.

2. **Handler-error path** — `daemon.rs:157`: called when `handle_brief` itself returns
   `Err`. Produces a `VerdictKind::Failed` with the error message as reason. This path
   is valid under the FSM model — it maps to an `AbortRequested`/infrastructure-failure
   event causing transition to `Failed`.

3. **DOL composition path** — `daemon.rs:compose_meta_verdict` (not shown but referenced
   at line 667): synthesizes the meta-brief's terminal verdict from its children. This
   path correctly fires at the meta-brief's logical terminal point.

The FSM eliminates path (1) entirely. Path (2) becomes a `BriefEvent::AbortRequested`
or a synthetic abort event injected by the daemon on infrastructure error. Path (3)
is unaffected in the first slice (composition follow-up).

### 1.3 Current `active_briefs` set mechanics

`ACTIVE_BRIEFS_SET = "agentry:active_briefs"` — `daemon.rs:33`.
SADD on brief receive (line 96); SREM after `handle_brief` + DOL hook complete (line 184).
The FSM subsumes this set: a brief is "active" iff current state is non-terminal.
The `active_briefs` SADD/SREM pattern is the crude predecessor of the FSM's
`Submitted → ... → terminal` lifecycle.

### 1.4 Daemon's current async architecture — what the FSM consumer changes

Today: `read_next_brief` → `tokio::spawn(handle_brief)`. One Tokio task per brief.
The task calls `join_all` on roles (fan-out within the brief). The task terminates
when `handle_brief` returns.

With the FSM: the daemon needs a second `tokio::spawn`'d loop — the **state-machine
consumer** — that subscribes to trace events (`agentry:brief:{id}:trace`) and
projects state transitions to `agentry:brief:{id}:state_log` + `agentry:brief:{id}:state`.

Critical difference: today, `handle_brief` is both the *orchestrator* (drives role
fan-out) and the *observer* (accumulates verdicts). Under B4, the daemon retains the
orchestrator role and ALSO becomes the FSM projector. The state-machine consumer loop
is a second async loop reading the trace stream; the existing `handle_brief` task
becomes the source of the trace events.

This is NOT a simple refactor of `handle_brief`. The daemon now has two concurrent
loops per brief:
- The orchestrator task (current `handle_brief`, drives role fan-out)
- The FSM projector task (new, reads trace events, projects state transitions)

**Back-pressure question**: the FSM projector uses `XREAD BLOCK` on
`agentry:brief:{id}:trace`. There is no consumer group today (`read_next_brief` uses
plain `XREAD` with a positional offset). Redis streams provide no built-in
back-pressure; if the projector falls behind, trace events accumulate in Redis. The
severity depends on event volume: each role run produces O(10–1000) trace events. For
briefs with many rework iterations, the stream can grow large.

Consumer group vs. plain `XREAD BLOCK`: a consumer group (`XGROUP CREATE` +
`XREADGROUP`) provides delivery guarantees (events are not lost if the consumer
crashes; PEL tracks unacknowledged entries). Plain `XREAD BLOCK` with a positional
offset is simpler but requires the daemon to persist the last-seen ID across restarts
to avoid reprocessing. The FSM projector needs the positional offset approach
(replay from 0-0 to reconstruct state on daemon restart) — this is the event-sourced
replay the captain proposed.

### 1.5 SETNX dedup — why it's load-bearing today and what replaces it

`append_verdict_idempotent` — `redis_io.rs:98`: uses `SET key 1 NX EX 86400` on
`agentry:verdict:emitted:{brief_id}`. This is a 24h idempotency gate. The sentinel
key AND the stream entry are written non-atomically (SET then XADD) — there is a
tiny window where the sentinel is claimed but the XADD fails (Redis OOM or network
partition). Under the FSM, verdict emission is driven by terminal-state transition,
which is itself an atomic XADD to the state log. The sentinel becomes unnecessary if
the transition function guarantees exactly-one terminal state transition.

### 1.6 `reworks_used` — current inner-loop budget

`handle_brief` tracks `reworks_used: u32` (daemon.rs:277) against `team.max_retries`
(orchestrator-types/src/team.rs). This is per-brief-execution, in process memory.
It is NOT persisted across daemon restarts (forensic gap: a daemon crash during a
rework cycle resets the counter to 0).

B8 locks in `attempt: u32` on the FSM state record with `cap=3`. The inner rework
counter and the outer retry counter are unified into one `attempt: u32`. The
current `reworks_used` in `handle_brief` becomes the FSM's `attempt` field in the
`Authoring` or `Reworking` state.

### 1.7 `RoleState` enum — private, not a pub type

`enum RoleState { Pending, Running, Shipped, Failed }` — `daemon.rs:203`. This is
private to `daemon.rs`, not exported. It should NOT be confused with the proposed
`BriefState`. `RoleState` is the per-role-per-execution view; `BriefState` is the
per-brief-per-lifetime view. They are orthogonal.

### 1.8 Event name collision check

Captain's proposed events: `CoderStarted, CoderDone, AcVerifierDone, ReviewerDone,
ShipperDone, CiResult, RebaseStarted, Rebased, RetryRequested, AbortRequested, BudgetExhausted`.

Existing `EventKind` variants in event.rs:58:
`Event, ToolCall, Message, Log, Finding, Status, Done`.

No name collision at the Rust type level — these live in different enums. The
semantic overlap to flag: `EventKind::Done { verdict: EventVerdict::Shipped }` on the
trace stream is the signal that becomes `BriefEvent::CoderDone(EventVerdict::Shipped)`
at the FSM layer. The daemon's FSM projector is the translation boundary: it reads
`EventKind::Done` from the trace and maps it to the corresponding `BriefEvent`.

The translation is role-aware: the daemon knows which role emitted the `Done` event
(the trace entry carries `agent_id`). So `CoderDone` vs `ReviewerDone` is determined
by role name, not by the event variant. This means `BriefEvent` variants are
constructed by the projector, not by the agent.

---

## §2 Proposed spec contribution — rust-systems lens

### `brief_lifecycle.md`

#### `BriefState`

The exhaustive Rust enum that represents a brief's position in the lifecycle.
Every variant maps to a named state in the FSM; terminal variants carry no
payload. Lives in `crates/orchestrator-types` (pure types, no async).

- depends on: BriefId
- depends on: BriefEvent
- depends on: Ts

State set:

```
Submitted
Authoring { agent_id: String, started_at: Ts, attempt: u32 }
Verifying { attempt: u32 }
Reviewing { attempt: u32 }
Shipping  { attempt: u32 }
Watching  { pr_number: u32, head_sha: String, attempt: u32 }
Reworking { iteration: u32, attempt: u32 }
Shipped          // terminal
Failed           // terminal
ReworkedOut      // terminal: attempt >= ATTEMPT_BUDGET_CAP
Aborted          // terminal
```

`Reworking` carries `iteration` (rework cycle within one attempt), `attempt`
(unified outer-retry counter). `ReworkedOut` is the terminal when
`attempt >= ATTEMPT_BUDGET_CAP` on a `RetryRequested` event.

Every non-terminal variant carries `attempt: u32`. This is the unified counter
that survives inner-loop rework cycles and outer `RetryRequested` events.

Invariant: only one transition into each terminal state is valid. Once in a
terminal state, `handle(state, event)` returns `Err(InvalidTransition)` for
all events. The projector MUST NOT call `handle` on terminal states.

#### `BriefEvent`

The exhaustive Rust enum of events the daemon's FSM projector translates from
trace events. Lives in `crates/orchestrator-types` (pure types, no async).

- depends on: EventVerdict
- depends on: ReviewFinding

```
CoderStarted { agent_id: String }
CoderDone(EventVerdict)
AcVerifierDone(EventVerdict)
ReviewerDone { verdict: EventVerdict, findings: Vec<ReviewFinding> }
ShipperDone { pr_number: u32, head_sha: String }
CiResult { passed: bool, head_sha: String }
RebaseStarted
Rebased { head_sha: String }
RetryRequested { reason: String }
AbortRequested { reason: String }
BudgetExhausted
```

Note: `BriefEvent` is NOT `EventKind`. `EventKind` is the agent wire type.
`BriefEvent` is the FSM input type. The daemon's projector translates
`EventKind::Done { verdict, .. }` + role-name context into `BriefEvent::CoderDone`
or `BriefEvent::ReviewerDone`, etc.

#### `InvalidTransition`

The error type returned by `handle` when an event is invalid for the current state.
Lives in `crates/orchestrator-types` (pure types, no async).

```
pub struct InvalidTransition {
    pub state: String,
    pub event: String,
}
```

String representations avoid circular type dependencies for display purposes.
The projector logs `InvalidTransition` as a warning and discards the event.

#### `BriefStateRecord`

The envelope written to the state log stream on each transition. Lives in
`crates/orchestrator-types` (pure types, no async).

- depends on: BriefId
- depends on: BriefState
- depends on: BriefEvent
- depends on: Ts

```
pub struct BriefStateRecord {
    pub brief_id: BriefId,
    pub state: BriefState,
    pub triggering_event: BriefEvent,
    pub at: Ts,
    pub parent_brief_id: Option<BriefId>,
    pub composition_role: Option<String>,
}
```

`parent_brief_id` and `composition_role` are present on every record from day
one (B3: composition-ready data model). They are `None` for non-composition briefs.
`composition_role` is `Option<String>` not `Option<CompositionRole>` in v1 because
`CompositionRole` is a composition-slice type not yet defined — using `String`
prevents a forward dependency.

### `brief_state_stream.md`

#### `BriefStateStream`

The Redis stream contract for per-brief state transitions. Each brief has
exactly one state log stream and one projected current-state key.

- depends on: BriefStateRecord
- depends on: BriefId

Redis key schema:
- `agentry:brief:{brief_id}:state_log` — append-only XADD stream. Each entry has
  one field `record` whose value is `serde_json::to_string(&BriefStateRecord)`.
  Stream is never trimmed by the daemon; GC is a separate operator concern.
- `agentry:brief:{brief_id}:state` — projected current state. A Redis STRING (not
  stream) written atomically alongside the XADD via Lua script. Value is
  `serde_json::to_string(&BriefState)`. Consumers that need only current state
  GET this key instead of XREVRANGE-ing the full log.

Ordering guarantee: the state log is per-brief. Within one brief, XADD is
append-only and Redis streams are ordered by server-generated ID. No
cross-brief ordering guarantee is implied or required.

Replay semantics: on daemon restart, the projector replays the state log from
`0-0` to reconstruct current `BriefState`. The projected `state` key is
authoritative for current state when it exists; the log is authoritative for
history and is the recovery source when the `state` key is absent (e.g. after
Redis OOM eviction of a STRING key while the stream survives via `maxmemory-policy
noeviction` or stream-specific TTL).

#### `BriefStateProjector`

The async component that subscribes to the trace stream, maps `EventKind` +
role context to `BriefEvent`, calls `handle`, and writes `BriefStateRecord`.
Lives in `crates/orchestrator-runtime` (async, depends on Redis).

- depends on: BriefState
- depends on: BriefEvent
- depends on: BriefStateRecord
- depends on: BriefStateStream

`BriefStateProjector` is NOT a port trait. It is a concrete struct. The FSM
transition function `handle` is pure and separately testable; `BriefStateProjector`
is the async shell that drives it.

Atomic write requirement: the XADD to `state_log` AND the SET to `state` key MUST
be atomic. A Lua script is the correct mechanism (see §3). If Lua is unavailable,
the projector falls back to a non-atomic write and logs a warning — but this is an
operator misconfiguration, not a normal code path.

### `brief_retry_budget.md`

#### `AttemptBudgetCap`

The compile-time constant cap on the unified `attempt: u32` counter. Lives in
`crates/orchestrator-types`.

```rust
pub const ATTEMPT_BUDGET_CAP: u32 = 3;
```

Per CLAUDE.md §8: threshold is a `const`. No ceiling file, no allowlist, no
`--update-baseline`. To raise the cap, edit this constant in a reviewed PR that
argues the new value. The cap applies to the unified counter: inner-loop rework
cycles AND outer `RetryRequested` events both increment `attempt`. When
`attempt >= ATTEMPT_BUDGET_CAP` and a `RetryRequested` event arrives, the FSM
transitions to `ReworkedOut` (terminal), not `Authoring`.

The manual override flag (captain's proposal) is NOT a second constant or a
runtime config key. It is a `RetryRequested { reason: String, override_budget: bool }`
field. The projector ignores `override_budget` unless it is explicitly handling
operator-escalation events from a privileged source. This prevents accidental
budget bypass from malformed events.

---

## §3 Non-negotiables — rust-systems lens

**N1: `BriefState` and `BriefEvent` live in `orchestrator-types`, not `orchestrator-runtime`.**
The transition function `handle(state, event) -> Result<BriefState, InvalidTransition>`
is pure — no I/O, no async. Pure types with a pure function belong in the types crate.
The async projector that drives the function lives in `orchestrator-runtime`. This split
is identical to how `VerdictKind` and `Verdict` live in `orchestrator-types` while
`append_verdict_idempotent` lives in `orchestrator-runtime/src/redis_io.rs`.

**N2: No FSM library dependency (statig, rust-fsm, or similar). Hand-rolled stays.**
Rationale for the `statig` question raised in the task brief: `statig` targets
hierarchical FSMs (HSMs) with state inheritance and entry/exit actions. The captain
proposed a flat enum + transition function today. When composition ships (B3 follow-up),
the hierarchy is brief-parent/brief-child, NOT state-parent/state-child. Composition
does not require an HSM — it requires a query across multiple independent flat FSMs.
Importing `statig` now couples the type system to a library whose abstractions don't
match the actual hierarchy. The flat hand-rolled enum is correct and sufficient; the
extension point for topology-specific phases is `Extension(name: String, data: serde_json::Value)`
— an open variant, not an HSM sub-state. **No rewrite needed when composition ships.**

**N3: Atomic `state_log XADD + state SET` via Lua script. Non-negotiable.**
Without atomicity, a daemon crash between XADD and SET leaves the projected `state`
key stale while the log has the new record. The replay path recovers correctness
(projector rebuilds state from the log on restart), but any consumer reading the `state`
key between the XADD and the SET sees stale state. Dashboard reads will show wrong
state during this window. The Lua path is `EVALSHA`-cached after first load. Redis OOM
that kills a Lua script mid-execution aborts the entire script atomically (Redis Lua
scripts are atomic at the Redis level) — the XADD and SET either both happen or neither.
This is NOT a correctness risk from OOM; it is a liveness risk (the transition is not
persisted). The projector retries on the next trace event.

**N4: `BriefEvent` must NOT shadow or wrap `EventKind`. They are distinct types at distinct layers.**
`EventKind` is the agent wire type (stdout NDJSON). `BriefEvent` is the FSM input type.
The projector is the translation boundary. Merging them would couple the FSM's type
system to the wire format, making it impossible to change the wire format without
changing the FSM, and vice versa. Today `EventKind` has `Done`, `ToolCall`, `Message`,
`Log`, `Finding`, `Status` — only `Done` events are FSM inputs. The other variants are
not FSM-relevant. A merged type would require the transition function to handle
non-FSM variants or `unreachable!()` them — both are wrong.

**N5: The projected `state` key must use `SET ... KEEPTTL` (or no TTL) — not a fixed TTL.**
The 24h TTL on `agentry:verdict:emitted:{brief_id}` (the current SETNX sentinel) was
a pragmatic choice to prevent sentinel key accumulation. Under the FSM, the `state` key
is the projected current state — it must survive 24h if a brief is long-running. Using
a fixed TTL risks evicting the `state` key while the brief is still active, forcing
unnecessary log replay on every read. Instead: write the `state` key with no TTL, and
clean it up as part of the brief's terminal GC pass (same pass that handles workspace
teardown). The state log key follows the same policy.

**N6: `ATTEMPT_BUDGET_CAP` is a typed `const u32` in `orchestrator-types`, not a field in `TeamTopology`.**
CONTESTED — `clean-arch` may prefer the cap to be per-topology configuration (stored in
`TeamTopology.max_retries`). The rust-systems argument: `TeamTopology` already has
`max_retries: u32` (used by `reworks_used` today in daemon.rs:465). The problem is
that `max_retries` is a per-team runtime value, not a compile-time invariant. This
enables operators to set `max_retries = 100` in a team definition and grind forever.
The FSM needs a hard floor and ceiling that is not overridable by topology definition.
The resolution: `ATTEMPT_BUDGET_CAP` is the hard ceiling const; `TeamTopology.max_retries`
can be <= `ATTEMPT_BUDGET_CAP` as a per-team soft cap, but the FSM enforces the hard
ceiling regardless of team config. If `team.max_retries` > `ATTEMPT_BUDGET_CAP`, the
daemon warns at dispatch time and uses `ATTEMPT_BUDGET_CAP`.

**N7: Consumer group for the state-machine projector, not plain `XREAD BLOCK`. CONTESTED.**
The current `read_next_brief` uses plain `XREAD BLOCK` with a positional offset. This
is adequate for the briefs stream (single consumer, no competing readers). For the
trace-stream projector, the clean-arch lens may prefer the simplicity of plain `XREAD`.
The rust-systems argument: the trace stream is written by agents (high frequency) and
the projector is the only consumer. A consumer group provides:
1. PEL: unacknowledged entries survive daemon crash; the projector ACKs entries only
   after the state transition is persisted to `state_log`. No transition is lost.
2. The consumer group NOACK variant can be used if replay-from-log semantics are
   preferred over PEL tracking (simpler, recovers from crash by replaying).

The contested position: if the projector always replays from `0-0` on restart, the PEL
provides no benefit and the consumer group adds operational complexity (requires
`XGROUP CREATE` at startup). The non-contested outcome: at minimum, the projector MUST
persist its last-consumed trace event ID to Redis (in `agentry:brief:{id}:state_projector_id`
or equivalent) so it does not replay the full trace on every restart for long-running briefs.
Whether this persistence uses a consumer group PEL or an explicit key is an implementation detail.

**N8: The `Extension` variant must use `#[non_exhaustive]`-compatible patterns.**
When topology-specific extension variants ship (B6), existing `match` arms in the
codebase that handle `BriefState` and `BriefEvent` must not silently ignore new variants.
The Rust compiler enforces exhaustive matching on enums. Adding `Extension` variants
later will produce compile errors at every match site — which is correct and desired.
Do NOT add a `#[non_exhaustive]` attribute to `BriefState` or `BriefEvent`; this would
suppress the compiler's exhaustiveness check and allow silent omissions. The compile errors
ARE the safety net. Every match site must explicitly handle `Extension(name, data)` with a
logged-and-discarded or delegated arm when extension support ships.
