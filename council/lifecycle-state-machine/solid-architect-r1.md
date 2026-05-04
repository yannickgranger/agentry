# solid-architect lens — r1

**Council:** lifecycle-state-machine
**Issue:** #246 — substrate brief lifecycle state machine
**Lens author:** solid-architect
**Date:** 2026-05-02

---

## §1 Archaeology — solid-architect lens

### Daemon's current responsibility set (SRP audit)

`crates/orchestrator-runtime/src/daemon.rs` is 73.6 KB. Reading it yields the following distinct responsibilities packed into one module:

1. **Brief intake loop** — XREAD from `agentry:briefs`; convert to `Brief`; guard concurrent-brief semaphores per project. (`daemon.rs:90-199`)
2. **Project-scoped concurrency** — fetch project cap, allocate Semaphore per project slug, gate spawns. (`daemon.rs:107-134`)
3. **DAG walk + role orchestration** — `handle_brief` owns the entire inner loop: ready-set computation, setup phase, `join_all` fan-out, outcome aggregation, rework rewind, failure short-circuit. (`daemon.rs:223-561`)
4. **Implicit FSM state management** — `HashMap<RoleRef, RoleState>` in process memory, `reworks_used: u32`, `team_shipped: bool`. These are non-persisted. (`daemon.rs:275-285`)
5. **Workspace allocation and teardown** — `workspace::allocate`, `workspace::destroy_with_disposition`. (`daemon.rs:311-325`, `590-610`)
6. **Verdict emission** — `append_verdict_idempotent` called inline after every role outcome AND on error path. (`daemon.rs:394-406`, `157-178`)
7. **DOL meta-brief lifecycle** — `dol_on_brief_terminal`, `on_all_children_resolved`, `compose_meta_verdict`, `compose_verdict_parts` — 250+ lines of children/verifier coordination. (`daemon.rs:614-894`)
8. **Chain-trigger dispatch** — `finalize_shipped_team`, `collect_chain_paths`, `load_next_brief`. (`daemon.rs:569-612`)
9. **Boot-time orphan branch sweep** — `workspace::sweep_orphan_branches` called at startup. (`daemon.rs:44-47`)
10. **Watchdog + projector spawn** — `tokio::spawn` for watchdog and projector sub-tasks. (`daemon.rs:59-72`)

**SRP verdict:** the daemon already has at least 8 distinct reasons to change. Adding "FSM consumer + state writer" — responsibility 11 — crosses into CCP territory rather than SRP territory IF it is truly an event-driven co-resident concern. See CCP analysis below.

### Current implicit state inventory — CCP analysis

The grill transcript identifies 9+ state locations (transcript "Adjacent observation" section). Applying CCP: which locations change for the SAME reason?

| State location | Change driver | CCP group |
|---|---|---|
| `agentry:active_briefs` Redis SET | Brief starts/ends | Brief-lifecycle |
| `agentry:verdict:emitted:{id}` SETNX sentinel | First verdict emission | Brief-lifecycle |
| `agentry:verdicts` stream | Terminal verdict | Brief-lifecycle |
| `agentry:brief:{id}:verifier_verdict` / `verifier_pending` | DOL verifier resolution | DOL-meta-lifecycle |
| `agentry:brief:{id}:children_pending` / `children_verdicts` | DOL child aggregation | DOL-meta-lifecycle |
| `HashMap<RoleRef, RoleState>` in process memory | DAG-walk step | DAG-execution (not persisted) |
| `reworks_used: u32` in process memory | Rework gate | DAG-execution (not persisted) |
| Forge state (PR, branch, mergeable) | Git/forge ops | External |
| Workspace directory presence | Workspace lifecycle | Workspace |

CCP analysis: "Brief-lifecycle" group (active_briefs + SETNX + verdicts) changes whenever the lifecycle model changes. The proposed FSM is the single writer that replaces all three. That is CCP-correct. The DOL-meta-lifecycle group is a SEPARATE change driver (composition semantics); it should remain addressable separately.

**Key finding:** adding an FSM consumer to the daemon does NOT overload SRP if it replaces the scattered "Brief-lifecycle" state group, and if the DOL-meta-lifecycle concern remains a distinct module. The daemon's violation today is not that it will own the FSM — it is that its implicit state management is spread across 10 locations with no authoritative projection.

### Existing ISP boundary at the trace-stream interface

Today, roles emit to `agentry:brief:{id}:trace` via `EventKind` (8 variants: `Event`, `ToolCall`, `Message`, `Log`, `Finding`, `Status`, `Done`). The daemon DOES NOT subscribe to this stream to make decisions — it is pure audit/mirror. The daemon drives role-spawning from the DAG walk in process memory.

Under the proposed FSM, the daemon would additionally subscribe to trace events that are meaningful for state transitions. This creates a NEW interface boundary between "events that drive FSM transitions" and "events that are pure trace data."

ISP concern: today's `EventKind` is a single fat enum. The FSM transition function would only care about a strict subset: `Done { verdict }` (from roles) and `Status { stuck }` (from watchdog). Adding a dependency on `EventKind` for the FSM consumer means the FSM consumer sees `Finding`, `Log`, `ToolCall`, `Message` — none of which drive transitions. This is the ISP violation: "don't force users of the FSM contract to depend on types they don't need."

Concrete measurement: FSM transition consumer needs 2 of 8 EventKind variants (25%). That is exactly the CRP threshold (25%) — marginal by the principle.

### OCP for `Extension(name, ...)` (B6)

The transcript proposes: `Extension(name, ...)` as a variant for topology-specific phases, analogous to FENCE_MATRIX. OCP check: adding a new extension currently requires:
1. Adding a row to the extension transition table (ONE place)
2. Possibly adding match arms for the new extension in any code that exhausts `BriefState`

Point 2 is the OCP risk. A `BriefState::Extension(name, payload)` variant means every `match state { ... }` in the codebase needs an `Extension` arm. If the arm is `_ => default_handler()` or the extension-specific logic is fully data-driven, this is OCP-safe. If code that processes `BriefState` must enumerate extensions by name to dispatch logic, it is NOT OCP-safe — it becomes open-by-convention, not open-by-design.

### God-state risk in `BriefState` variant payload

Captain's proposed variants carry inline data:
- `Authoring { agent_id: String, started_at: Ts, attempt: u32 }` — 3 fields
- `Verifying { ... }` — unknown field count
- `Reviewing { ... }` — unknown
- `Watching { pr_number: u32, head_sha: String }` — 2 fields
- `Reworking { iteration: u32, max: u32, target: ReworkTarget }` — 3 fields (including nested type)

Threshold: variants with >3 fields begin to encode "what happened during this phase" not just "what phase is this." The `BriefState` enum should answer "where is the brief?" — not "what do I know about what happened in each phase?" The latter belongs in `BriefMetadata`.

---

## §2 Proposed spec contribution — solid-architect lens

### For `brief_lifecycle.md`

Every `##` heading maps to a pub Rust type.

```markdown
## BriefState

The current phase of a brief in the substrate. Written exclusively by
the daemon's FSM consumer. The enum answers one question: "where is this
brief?" — it carries only the data needed to answer that question for the
current phase, not a full history of what happened in prior phases.

Terminals are `Shipped`, `Failed`, `ReworkedOut`, `Aborted`. A brief
reaching a terminal state never transitions again. The daemon emits a
single `agentry:verdicts` entry on the first terminal transition.

Active variants carry the MINIMUM identifying context for the current
phase: `Authoring` carries `agent_id` and `attempt`; it does NOT carry
`started_at` (that is `BriefStateRecord.at`). No variant carries more
than 3 fields. If a phase requires more context, the additional data
lives in `BriefMetadata`, not on the variant.

`Extension` carries a topology-specific phase name and an opaque payload.
The FSM's transition table resolves extension transitions by name lookup —
the `handle` function does NOT match on extension names; it delegates to
a per-extension table so that adding a new extension is one table row,
not a new match arm.

## BriefEvent

The set of typed inputs to the FSM transition function. Only events that
CAUSE state transitions belong in this enum; pure trace events (log lines,
tool calls, findings, messages between roles) are NOT represented here —
those remain in `EventKind`.

The disciplined split: `BriefEvent` is the narrow interface the FSM
consumer exposes. Every other trace event kind is opaque to the FSM.

## BriefStateRecord

A single append entry on `agentry:brief:{id}:state_log`. Carries the new
state, the event that caused the transition, and composition-readiness
fields. The `parent_brief_id` and `composition_role` fields are present
on every record from day one (B3 decision: data-model-ready even if the
reactive composition layer is a follow-up EPIC).

Fields:
- `brief_id: BriefId`
- `state: BriefState`
- `caused_by: BriefEvent`
- `at: Ts`
- `parent_brief_id: Option<BriefId>`
- `composition_role: Option<String>`

CRP note: `parent_brief_id` and `composition_role` are on every record
because the state stream is the composition layer's primary input. Any
consumer of `BriefStateRecord` that needs to answer a lineage query needs
both fields. Consumers that don't need lineage can ignore them — they are
`Option` and zero-cost to skip. This is NOT a CRP violation because the
composition-layer consumers are the primary audience of `BriefStateRecord`.

## BriefMetadata

Mutable audit fields about a brief that are NOT part of the phase
identity. Separating this type from `BriefState` keeps the enum clean
and prevents variant payload inflation.

Fields owned here (NOT on `BriefState` variants):
- `attempt: u32` — current attempt count, monotonically increasing
- `attempt_cap: u32` — configured maximum (default 3, operator-overridable)
- `reworks_used: u32` — inner-loop rework count within the current attempt
- `max_retries: u32` — inner rework cap (from TeamTopology)
- `agent_id: Option<String>` — agent currently assigned, if in Authoring/Verifying/Reviewing

## InvalidTransition

Error type returned by the pure `handle` function when an event is not
valid for the current state. Carries `from_state`, `event`, and
`reason` so the daemon can emit a structured log and decide whether to
surface the event as an operator escalation or silently discard it
(idempotent re-delivery of already-processed events must not error).
```

### For `brief_state_stream.md`

```markdown
## BriefStateStream

The Redis append-only stream contract for brief lifecycle state
transitions. Stream key: `agentry:brief:{id}:state_log`. One XADD per
`BriefStateRecord`. The daemon is the ONLY writer.

Projection: `agentry:brief:{id}:state` is a single Redis key holding
the JSON-serialised `BriefState` of the current (latest) transition.
Written by the daemon atomically with the XADD so point-in-time readers
can use either the projection (fast) or replay from the stream (authoritative).

Replay semantics: on daemon crash mid-transition, the stream is the
source of truth. The daemon reads `XREVRANGE ... COUNT 1` on startup
(per brief in `agentry:active_briefs`) to recover the latest committed
state before resuming.

`agentry:active_briefs` is deprecated by this context: a brief is
"active" iff its current projected `BriefState` is non-terminal.
Consumers migrating from SISMEMBER on active_briefs to a state
projection read are a follow-up wire change.
```

### For `brief_retry_budget.md`

```markdown
## RetryBudget

The unified attempt counter for a brief. Inner-loop rework (reviewer
Blocker → coder rework) and outer-loop retry (terminal Failed →
RetryRequested → Authoring) share ONE `attempt: u32` counter, ONE cap.

The cap is `attempt_cap: u32`, default 3. Default lives in the FSM
module as a `pub const DEFAULT_ATTEMPT_CAP: u32 = 3`.

Rationale for sharing inner + outer: from the operator's perspective,
"how many times has this work been attempted?" is a single observable.
Splitting into two counters would let a brief exhaust inner reworks
independently of outer retries — producing more total attempts than the
operator expects without a cap escalation.

ISP note: consumers that need only "is this brief still within budget?"
call `RetryBudget::within_cap()` without needing to inspect the inner
vs outer breakdown. The breakdown (reworks_used, outer_retries_used)
lives in `BriefMetadata` for consumers that need it.

The cap value of 3: today's X.0 run needed 5 attempts. Cap=3 would have
produced operator escalation after v3 rather than grinding to v5. The
override flag preserves operator agency for known-hard problems. Council
should confirm 3 as default.
```

---

## §3 Non-negotiables — solid-architect lens

### N1 — FSM transition function is pure; no I/O (SRP)

`fn handle(state: &BriefState, event: &BriefEvent) -> Result<BriefState, InvalidTransition>` must have zero I/O. It takes state + event, returns new state or error. The daemon's event-consumer loop owns all Redis reads and writes; it calls `handle()` and then writes the result. The function must compile in `orchestrator-types` (zero Redis dependency) — this is what makes it unit-testable without a Redis instance and what grounds the SDP layering.

**CONTESTED:** rust-systems may want the FSM logic in `orchestrator-runtime` to be co-resident with the event loop. The counterargument: moving it to `orchestrator-types` (I=0.0, fully stable) means the transition table is a type-system-level contract, not an implementation detail. The daemon imports it; nothing in `orchestrator-types` imports the daemon. SDP is preserved.

### N2 — BriefEvent is a narrow type, not a re-export of EventKind (ISP)

`BriefEvent` must be defined independently of `EventKind`. The daemon's FSM consumer translates from `EventKind` to `BriefEvent` at the subscriber boundary. This translation layer is the ISP seam: FSM consumers depend only on `BriefEvent`; they are not compiled against the full `EventKind` enum. Adding a new trace event kind (e.g. a new Status sub-type) does not force a recompile of FSM-only consumers.

Concrete: `BriefEvent::RoleDone { role: RoleName, verdict: EventVerdict }` is NOT the same type as `EventKind::Done { verdict, reason }`. The daemon maps one to the other. The FSM doesn't know about `DoneReason`.

### N3 — BriefState variant payload must not exceed 3 fields; excess belongs in BriefMetadata (Anti-god-state)

Any variant that needs more than 3 identifying fields is encoding phase history, not phase identity. That data belongs in `BriefMetadata`, fetched separately when needed. This rule is enforced structurally at code review: a PR that adds a 4th field to a `BriefState` variant is rejected in favor of promoting the field to `BriefMetadata`.

### N4 — Extension variant dispatch is table-driven, not match-arm-driven (OCP)

The `BriefState::Extension(name, payload)` variant is OCP-safe only if the transition logic for extensions is resolved via a data-driven table lookup (a `HashMap<&str, ExtensionTransitionFn>` or equivalent), not via `match name { "foo" => ..., "bar" => ..., _ => ... }`. Adding a new topology-specific phase is one table insertion. Every exhaustive match on `BriefState` has exactly ONE `Extension` arm — `Extension(_, _) => dispatch_extension(name, payload, &table)`.

**CONTESTED:** clean-arch may argue that table-driven dispatch is over-engineering at this scale (today: 0 extension phases). This lens maintains the rule because the FENCE_MATRIX analogy cited in B6 is exactly this pattern — and the FENCE_MATRIX has already been extended multiple times without touching surrounding code.

### N5 — lifecycle-state-machine is its own bounded context (CCP/SRP)

The FSM types (`BriefState`, `BriefEvent`, `BriefStateRecord`, `BriefMetadata`, `InvalidTransition`, `RetryBudget`) belong in a dedicated module or crate, not merged into `orchestrator-types`'s existing modules (`brief.rs`, `verdict.rs`). The reason to change for `BriefState` (lifecycle semantics change) is different from the reason to change for `Brief` (brief schema changes) or `Verdict` (terminal record format changes). Co-locating them in the same module means that a lifecycle semantics change forces recompile of all consumers of `Brief`.

Minimum viable boundary: a new `orchestrator-types/src/lifecycle.rs` module with its own `pub use` in `lib.rs`. Maximum viable boundary: a new `orchestrator-lifecycle-types` crate. This lens recommends the module boundary as the first step, with the crate split deferred until a second consumer crate (e.g. dashboard) needs to import lifecycle types without importing all of `orchestrator-types`.

### N6 — Stale-worktree GC (Case 6) belongs in a separate brief, not in this FSM EPIC (CCP)

Case 6 (workspace cleanup on `Failed` terminal) is a workspace lifecycle concern, not a brief state machine concern. The FSM provides the signal (`terminal Failed` transition); the workspace manager acts on it. These are two different reasons to change: the FSM changes when the state model changes; the workspace GC logic changes when deployment topology or retention policy changes. Bundling them is CCP-incorrect. This lens recommends filing a separate issue for Case 6 that wires a workspace lifecycle hook to the FSM's terminal-state event, but does NOT put the GC logic inside the FSM module.

**CONTESTED:** the grill notes Case 6 as a forensic motivator for the FSM EPIC. CCP says adjacency of motivation is not the same as adjacency of change drivers. The workspace module already exists at `workspace.rs`; the FSM just needs to emit a signal it can consume.

### N7 — Daemon's FSM consumer replaces, does not stack on, SETNX dedup (SRP)

The SETNX sentinel (`agentry:verdict:emitted:{id}`) must be removed in the same PR that lands the FSM. It must not coexist with the FSM as a second dedup layer. Two dedup mechanisms for the same invariant ("one terminal verdict per brief") is a split-brain — the second mechanism can silently override or conflict with the first. The FSM's terminal-state transition IS the dedup: transitioning to a terminal state is idempotent (already-terminal → same event = `InvalidTransition`), and the daemon emits the verdict exactly once, after the FSM confirms the transition.

