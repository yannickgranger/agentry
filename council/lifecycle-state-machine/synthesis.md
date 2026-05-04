# Synthesis — lifecycle-state-machine council

**Council:** lifecycle-state-machine-council
**Lenses:** rust-systems, solid-architect, clean-arch
**Rounds:** r1 (parallel-isolated) → r2 (cross-lens) → captain-synthesis
**Date:** 2026-05-02
**Captain:** Claude Opus 4.7

---

## Convergence map

After r2, the major architectural decisions converged across all 3 lenses. 6 narrow residuals remained, all settled by captain-synthesis with explicit citations to the lens that drove the resolution.

| r1 contest | r2 outcome |
|---|---|
| C1 — Monomorphization vs `Arc<dyn Trait>` for ports | **Generic bounds at `daemon::run`, monomorphization at the binary.** Single production adapter (Redis) + single test adapter (in-memory) doesn't warrant `Arc<dyn>`. All 3 agreed. |
| C2 — Table-driven Extension dispatch (FENCE_MATRIX precedent) | **Deferred table form** per clean-arch YAGNI. Solid withdrew N4 ("FENCE_MATRIX was a false precedent — extensions are a runtime registry, not policy data"). Extension variant exists; dispatch is `match ext_name { _ => Err("unknown") }` until first real extension lands. Table can be added when needed without restructure. |
| C3 — ATTEMPT_BUDGET_CAP shape | **Two-const ceiling** per rust-systems r2: `DEFAULT_ATTEMPT_CAP: u32 = 3` + `MAXIMUM_ATTEMPT_CAP: u32 = 10`. `TeamTopology.max_retries` honored within ceiling; dispatch rejects topologies declaring > MAXIMUM. Clean-arch's "pure config" position withdrew once the ceiling was articulated as a hard fence. |
| C-A (solid) — `handle_brief`'s `HashMap<RoleRef, RoleState>` split-brain | **Removed.** Replaced by FSM projector reads. The in-memory map currently duplicates state that will live in the FSM; keeping both reproduces the split-brain at a lower level. Critical CCP point — single source of truth for "what has this role done?" |
| C-B (rust-systems SC4 / clean-arch C-B) — BriefMetadata separate struct vs inline | **Inline on variant payload.** Clean-arch's pure-function invariant wins; no second Redis key. If a future "metadata view" is needed, derive from state record. |
| Terminal naming: `ReworkedOut` distinct vs `Failed { reason: BudgetExhausted }` | **Collapse into `Failed { reason }`** per rust-systems + solid. Flatter terminal set; reasons enum distinguishes (`BudgetExhausted`, `AbortRequested`, `AcceptanceFailed`, `Other`). |
| Crash recovery source (clean-arch C-C) | **Trace stream is authoritative event log.** State stream is derived projection — re-built from trace + cursor on projector restart. Event-sourced semantics. Captain originally framed state stream as primary; clean-arch's correction is right — replaying state stream alone misses events emitted between projector crashes and translation. |

---

## Final architectural decisions — locked

### Crate placement

- **`orchestrator-types`** (I=0.0, no Redis/tokio deps):
  - `BriefState` enum — pure data, all variants
  - `BriefEvent` enum — pure data, distinct from `EventKind`
  - `BriefStateRecord` struct — persisted shape (state + parent_brief_id + composition_role + timestamp)
  - `RetryBudget` struct — attempt counter + max
  - `Reason` enum — terminal-failure reasons
  - `ExtensionKind`, `Extension` variant carrier
  - `pub fn handle(state: &BriefState, event: &BriefEvent) -> Result<BriefState, InvalidTransition>` — pure transition function
  - `pub const DEFAULT_ATTEMPT_CAP: u32 = 3`
  - `pub const MAXIMUM_ATTEMPT_CAP: u32 = 10`

- **`orchestrator-runtime`** (depends on -types, owns Redis + tokio):
  - `EventSource` trait (port) — yields `BriefEvent` from a stream
  - `StateProjector` trait (port) — writes `BriefStateRecord` + appends to state stream
  - `RedisEventSource` (adapter) — `XREAD BLOCK` from `agentry:brief:{id}:trace`, translates `EventKind + role-name → BriefEvent`
  - `RedisStateProjector` (adapter) — Lua script for atomic `state_log XADD + state SET + state_projector_cursor SET`
  - `daemon::run<E: EventSource, P: StateProjector>(...)` — generic over ports, monomorphized at binary
  - Removes: `agentry:active_briefs` set, SETNX sentinel logic in `redis_io::append_verdict_idempotent`, `handle_brief`'s in-memory `RoleState` HashMap

- **`orchestratord` (binary)**:
  - Composition root: constructs `RedisEventSource` + `RedisStateProjector` + `Lua` scripts, passes into `daemon::run`
  - Verifies `MAXIMUM_ATTEMPT_CAP` ceiling at brief dispatch (`max_retries` validation)

### State machine shape

```rust
// orchestrator-types/src/lifecycle.rs (NEW module)

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefStateRecord {
    pub brief_id: BriefId,
    pub state: BriefState,
    pub parent_brief_id: Option<BriefId>,    // composition-ready (B3)
    pub composition_role: Option<String>,    // composition-ready (B3)
    pub at: Ts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum BriefState {
    Submitted,
    Authoring { agent_id: String, started_at: Ts, retry: RetryBudget },
    Verifying { retry: RetryBudget },
    Reviewing { retry: RetryBudget },
    Reworking { iteration: u32, max: u32, target: ReworkTarget, retry: RetryBudget },
    Shipping { pr_number: u32, head_sha: String, retry: RetryBudget },
    Watching { pr_number: u32, head_sha: String, retry: RetryBudget },
    Extension { name: String, data: serde_json::Value, retry: RetryBudget },
    Shipped,
    Failed { reason: Reason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryBudget { pub attempt: u32, pub max: u32 }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    BudgetExhausted,
    AbortRequested { actor: String, message: String },
    AcceptanceFailed { detail: String },
    DaemonError { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum BriefEvent {
    CoderStarted { agent_id: String },
    CoderDone(EventVerdict),
    AcVerifierDone(EventVerdict),
    ReviewerDone(EventVerdict, Vec<ReviewFinding>),
    ShipperDone { pr_number: u32, head_sha: String },
    CiResult { state: CiState, head_sha: String },
    RebaseStarted,
    Rebased { new_head_sha: String },
    RetryRequested { actor: String, reason: String },
    AbortRequested { actor: String, message: String },
    BudgetExhausted,
}

pub fn handle(state: &BriefState, event: &BriefEvent) -> Result<BriefState, InvalidTransition>;

pub const DEFAULT_ATTEMPT_CAP: u32 = 3;
pub const MAXIMUM_ATTEMPT_CAP: u32 = 10;
```

### Storage contract

- **`agentry:brief:{id}:state_log`** — append-only Redis stream, every transition is one XADD entry containing the new `BriefStateRecord` (JSON serialized). Ordering = stream-ID ordering = causal.
- **`agentry:brief:{id}:state`** — Redis key holding the latest `BriefStateRecord` JSON. Updated atomically with state_log XADD via Lua.
- **`agentry:brief:{id}:state_projector_cursor`** — Redis key holding the last trace-stream entry ID consumed by the projector. Used for crash recovery: on restart, projector resumes `XREAD` from this cursor.
- **`agentry:brief:{id}:trace`** — UNCHANGED (existing). Authoritative event log for crash replay. The projector reads from here, not from state_log.
- **`agentry:verdicts`** — UNCHANGED contract. The projector emits one entry on terminal-state transition (replaces today's per-role-outcome emission). SETNX dedup logic removed (terminal state is by definition single).

### Lua script for atomic projector write

```lua
-- KEYS: state_log_stream, state_key, cursor_key
-- ARGV: state_record_json, last_trace_id

redis.call('XADD', KEYS[1], '*', 'record', ARGV[1])
redis.call('SET', KEYS[2], ARGV[1])
redis.call('SET', KEYS[3], ARGV[2])
return 1
```

Atomic. Aborts cleanly on Redis OOM (no partial writes; only liveness risk on this transition, retried on next event).

### Daemon refactor

- `handle_brief` (currently in daemon.rs) shrinks dramatically: removes the per-role HashMap, removes inline verdict emission. Becomes the orchestrator that spawns roles and routes messages; FSM projector consumes events independently.
- Two concurrent async loops per brief:
  - **Orchestrator task**: spawns roles per topology DAG, routes inter-role messages
  - **Projector task**: subscribes to `agentry:brief:{id}:trace` via `XREAD ... 0-0 BLOCK ...`, translates each event to BriefEvent, calls `handle()`, on accepted transition writes via Lua script
- **Sequencing rule** (rust-systems r2): projector starts XREAD from `0-0` (not `$`) to avoid race window where events emitted before projector is scheduled are lost. Cursor in `state_projector_cursor` advances per processed event.
- **Crash recovery**: on daemon restart, for each brief in non-terminal state (read from `agentry:brief:{id}:state`), spawn a new projector task with cursor from `state_projector_cursor`. The projector replays from trace stream + last cursor; trace stream is authoritative event source. State stream is rebuildable.

### Three concepts → three specs

The grill proposed 3 candidate concepts; council confirmed the split.

- **`brief_lifecycle.md`** — the FSM itself: `BriefState`, `BriefEvent`, `Reason`, `handle` function, terminal semantics, the `attempt` budget tracking
- **`brief_state_stream.md`** — Redis storage contract: state_log + state key + cursor key, the Lua atomic write, crash recovery from trace stream
- **`brief_retry_budget.md`** — `RetryBudget` struct, `DEFAULT_ATTEMPT_CAP` + `MAXIMUM_ATTEMPT_CAP` consts, `RetryRequested` event mechanics, dispatch-time topology validation

---

## Per-lens unique contributions

### rust-systems

- **No FSM library.** Hand-rolled enum + match. No `statig`. Composition layer (when it ships) is brief-parent/brief-child hierarchy, not HSM sub-states. Flat enum stays correct.
- **Lua script non-negotiable** for atomic `state_log XADD + state SET`. Redis Lua aborts atomically on OOM — no partial-write correctness risk, only liveness on this transition (retried on next event).
- **XREAD cursor `0-0`, not `$`.** Captain missed this in the grill. Starting from `$` misses events emitted before the projector task is scheduled. `0-0` is race-free because trace stream is empty at dispatch time and all events arrive after.
- **Two-const ceiling** for budget: `DEFAULT_ATTEMPT_CAP: u32 = 3` + `MAXIMUM_ATTEMPT_CAP: u32 = 10`. Topology config honored within ceiling.
- **Three verdict-emission sites today** in daemon.rs identified: line 394 (role-outcome path, primary premature-shipped source — eliminated), line 157 (handler-error path — becomes synthetic AbortRequested event), DOL composition path (unaffected in slice 1).

### solid-architect

- **N8 — `attempt: u32` MUST be persisted in FSM state records, not in `handle_brief` stack frame.** Daemon crash mid-rework currently silently resets the budget. Real correctness gap, not observability.
- **C-A — `handle_brief`'s `HashMap<RoleRef, RoleState>` split-brain.** The in-memory map duplicates state that will live in the FSM; keeping both reproduces the split-brain at a lower level. Critical CCP point. Removal of this map is load-bearing.
- **`Extension` runtime registry, not static table.** FENCE_MATRIX was a false precedent — fences are policy data; extensions are runtime registry. Withdrawn N4 in r2.
- **`Failed { reason: ... }` flatter than `ReworkedOut` distinct terminal.** Reduces terminal variants; reasons enum carries the discriminant.
- **Stale-worktree GC (Case 6) is a separate brief.** Different change drivers from FSM; bundling violates CCP. The FSM emits the terminal-Failed signal; workspace module consumes it. (See "out of scope" below.)

### clean-arch

- **Composition root violation.** Today's `daemon::run` constructs its own `ConnectionManager` internally — library cannot be tested without Redis, binary cannot substitute adapters. The fix is to make the binary construct `RedisEventSource` + `RedisStateProjector` + the daemon takes generic bounds. Load-bearing refactor.
- **State stream is "command log," not pure event-sourcing.** This is acceptable but must be stated explicitly. Trace stream is the authoritative event log for crash replay; state stream is derived projection.
- **Crash recovery from trace stream, not state stream.** Replaying state stream alone misses events emitted between projector crashes and translation. State stream is rebuildable from trace + cursor.
- **EventSource yields stream of events** (not callback) — preserves backpressure semantics, projector controls flow.
- **`BriefMetadata` inline on variant payload** — preserves pure-function invariant; avoids second Redis key.

---

## Captain-synthesis decisions on residuals

### Extension dispatch — defer table

Solid r2 withdrew N4 ("FENCE_MATRIX is policy data; extensions are a runtime registry — different problem"). Rust-systems r2 accepted table form via `EXTENSION_TABLE`. Clean-arch r2 held YAGNI ("no table without a consumer on day one").

**Decision: defer table.** Extension variant exists in BriefState; dispatch is `match ext_name { _ => Err("unknown extension") }` until first real extension lands. The table can be introduced later without restructure (same pattern as cfdb ban rules — file the rule when needed). Clean-arch's YAGNI wins.

### Budget cap — two-const ceiling

Rust-systems r2 proposed two consts (DEFAULT=3, MAXIMUM=10). Clean-arch r2 accepted ("resolved via rust-systems' two-tier model"). Solid r2 leaned toward flatter shape but didn't insist.

**Decision: two-const ceiling.** Per rust-systems r2:
- `DEFAULT_ATTEMPT_CAP: u32 = 3` — applied if topology doesn't specify
- `MAXIMUM_ATTEMPT_CAP: u32 = 10` — hard ceiling enforced at dispatch
- `TeamTopology.max_retries` is honored within `MAXIMUM_ATTEMPT_CAP`; dispatch rejects topologies above ceiling with clear reason

This gives the substrate a fence (no runaway retries even if a topology author overspecifies) AND honors topology autonomy within the ceiling.

### handle_brief HashMap split-brain — removed

Solid r2's C-A point is critical. The current daemon's in-memory `HashMap<RoleRef, RoleState>` becomes a parallel state authority alongside the FSM. Two authorities = split-brain at lower level — exactly the disease the FSM is meant to cure.

**Decision: HashMap removed in the same PR that introduces the FSM.** `handle_brief` reads role state from the FSM projector (or directly from `agentry:brief:{id}:state` if synchronous), not from in-memory tracking. The daemon's orchestrator task spawns roles and routes messages; the projector task is the single source of truth for "what has this role done?"

### Crash recovery source — trace stream

Clean-arch r2's C-C correction is right. State stream replay misses events emitted between projector crashes and translation; trace stream is authoritative.

**Decision: trace stream is authoritative event log.** On daemon/projector restart:
1. Read all briefs in non-terminal state from `agentry:brief:{id}:state` (key list scan)
2. For each, spawn projector task with cursor from `agentry:brief:{id}:state_projector_cursor`
3. Projector replays from trace stream starting at cursor
4. State stream is implicitly rebuilt as transitions occur

State stream remains the operator-facing artifact (granular history); trace stream remains the source-of-truth event log.

---

## Outputs from this council

1. **3 specs** (this PR):
   - `specs/concepts/brief_lifecycle.md` — FSM enum + handle function + terminal semantics
   - `specs/concepts/brief_state_stream.md` — Redis storage + Lua atomic write + crash recovery
   - `specs/concepts/brief_retry_budget.md` — RetryBudget + cap consts + RetryRequested mechanics

2. **Status:** all `draft` initially. Graduate to `ratified` only when the implementation PR opens AND the specs' `##` headings map to actual Rust pub types (X.0 v3 lesson — graph-specs has no draft escape; ratified specs must mirror code).

3. **Council artifacts** at `council/lifecycle-state-machine/`:
   - `grill-transcript.md` — upstream user deliberation
   - `rust-systems-r1.md` + `rust-systems-r2.md`
   - `solid-architect-r1.md` + `solid-architect-r2.md`
   - `clean-arch-r1.md` + `clean-arch-r2.md`
   - `synthesis.md` (this file)

4. **Next step:** user audits the 3 specs. If approved, `/to-issues` decomposes the EPIC into vertical-slice briefs.

---

## Out-of-scope notes (forwarded to future work)

- **Reactive parent-from-child composition layer** — the data model is composition-ready (parent_brief_id + composition_role on every state record), but reactive transitions driven by child state changes are a follow-up EPIC slice.
- **Stale-worktree GC** (Case 6) — adjacent failure mode not subsumed by the FSM. The FSM emits terminal-Failed; the workspace module should consume it and clean up. Separate brief, not bundled into this EPIC. Solid r1 N: "different change drivers; bundling violates CCP."
- **`RetryRequested` operator-facing CLI** — operational detail (how does a human invoke a retry?). Not part of the FSM spec.
- **`agentry:active_briefs` set deprecation** — subsumed by FSM (a brief is "active" iff its current state is non-terminal). Removed in the same PR; consumers update.
- **#182 (runner pivot) workflow definition entanglement** — orthogonal. Workflows-as-data sits ABOVE the FSM; the FSM is the per-brief lifecycle layer beneath.
- **Dashboard rewrite** — existing dashboard reads `agentry:verdicts`; works under the new model unchanged. Granular state-stream consumption is opt-in for future dashboards.
- **Auditor / forensics tooling** — the state stream is the natural feed for "show me all briefs that hit BudgetExhausted"; tooling on top is post-FSM.

---

## Vertical slice candidates for /to-issues

Captain's preliminary breakdown (council/synthesis suggests but `/to-issues` formalizes):

- **L.1** — Lifecycle types in orchestrator-types (BriefState, BriefEvent, Reason, RetryBudget, BriefStateRecord, ExtensionKind, handle function, consts) + unit tests for transition table
- **L.2** — EventSource + StateProjector traits in orchestrator-runtime + Redis adapter implementations + Lua script + crash recovery
- **L.3** — Daemon refactor: monomorphize over ports; remove handle_brief's RoleState HashMap; spawn projector task per brief; remove SETNX sentinel and active_briefs set; verdict emission only on terminal-state transitions
- **L.4** — Cutover migration brief: drain in-flight, restart daemon under new model, verify
- **L.5** — Spec graduation brief: graduate the 3 specs from draft → ratified (after L.1-L.3 ship and pub types match headings)

Plus **L.6** — Stale-worktree GC (separate-brief candidate, listens to FSM terminal-Failed, cleans up). Out of EPIC scope per solid r1; could be filed as separate issue under #246's umbrella.

The cap=3 vs override question (whether `MAXIMUM_ATTEMPT_CAP=10` is the right ceiling) deserves operator confirmation before L.1 dispatch — captain proposed; council validated; user can adjust.
