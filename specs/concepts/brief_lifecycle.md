# Brief lifecycle

> Status: **ratified, post-#495-beta-b**. The phase-enum FSM has been
> collapsed to a generic topology walker. The pure FSM types and
> `handle` transition function live in
> `crates/orchestrator-types/src/lifecycle.rs`. Daemon wiring, the
> projection on Redis streams, and the persisted state-stream substrate
> live alongside this spec in `specs/concepts/brief_state_stream.md`.

The bounded context that owns *the position of a brief inside its
end-to-end shipping pipeline*. A brief is a single record that travels
from submission, through whatever per-team topology the operator
dispatched it into, to one of two terminals. The lifecycle captures
only what is *observable about the brief itself* — the topology node
whose role most recently reported, the accumulated multiset of
sibling verdicts that drives gate decisions, the retry counter, the
per-node run-data payload (a coder's agent id while it runs, the
PR number/head SHA once a shipper produces one, a coder-flagged
disagreement payload when a brief is parked for captain decide).
Per-agent execution detail (token usage, transcript paths, watchdog
signals) is not part of this concept; it lives in the agent contract
and event streams.

The FSM is a pure transition table: `handle(state, event,
walk_config, entry_node) -> new_state`. Wall-clock time, persistence,
agent dispatch, and human notification are layered by the daemon
caller. Keeping the table free of I/O is what makes the entire flow
replayable: the same event log projected against the same starting
state, the same `WalkConfig`, and the same `entry_node` will always
produce the same sequence of `BriefStateRecord`s. The L.2 brief
introduces an `EventSource` and `StateProjector` that consume this
FSM.

## BriefStateRecord

Persisted projection of one FSM step. Wraps a `BriefState` with the
`BriefId` it belongs to, the wall-clock `at` the daemon stamped when
the transition was processed, and two optional composition fields used
by planner-spawned children (`parent_brief_id`, `composition_role`)
to re-link a child brief to the parent it was authored under. The
projector writes one record per `handle` call; the dashboard streams
records and the resume path replays them in `at` order to rebuild the
in-memory state.

## BriefState

The position a brief occupies in the lifecycle. Sum type with **four**
variants: one entry state (`Submitted`), one workhorse non-terminal
state (`Walking`), and two terminals (`Shipped`, `Failed`).

`Walking` is the only non-terminal state after the collapse. It carries:

- `node_id: NodeId` — the role-name of the topology node whose most
  recent `RoleDone` shaped the current state. This doubles as the
  late-event fence reference: the lifecycle driver detects late
  events (a `RoleDone` for a node that is strictly upstream of
  `node_id` in the topology) by comparing the reporter to this field
  via `is_late_event`.
- `evidence: BTreeMap<NodeId, EventVerdict>` — the accumulated
  multiset of per-node verdicts across the entire walk. Every
  `RoleDone` folds its verdict into this map; `advance_walker`
  consults it on each step to decide whether each downstream node's
  gate (per `WalkConfig.node_configs[d]`) is satisfied by the
  evidence collected so far.
- `run_data: RunData` — the per-node variant payload. See `RunData`
  below for the variants and their lifetimes.
- `retry: RetryBudget` — the brief's retry counter. Rework-class
  outcomes (soft fails, CI failure) call `increment_or_fail` to
  bump `attempt` and either rebuild a fresh `Walking` at the entry
  node or short-circuit to `Failed{BudgetExhausted}`.

`Shipped` and `Failed` are the two terminals. `Shipped` carries no
payload — every shipped brief is a shipped brief. `Failed` carries a
`Reason` so dashboards can render a typed badge per failure mode
without parsing prose.

The legacy phase-specific variants (`Authoring`, `Verifying`,
`Reviewing`, `Reworking`, `Shipping`, `Watching`, `Extension`,
`AwaitingCaptainDecision`) were deleted in beta-b. Phase names are
now metadata on the topology (the role-name itself), not enum
variants. See the migration appendix at the bottom of this document.

## RunData

Per-node run-data carried by `BriefState::Walking` and (optionally)
by `BriefEvent::RoleDone`. Five variants tagged `kind` + snake_case:

- `none` — stateless node (verifier, reviewer, ci-watcher mid-poll).
- `coder { agent_id }` — coder container is alive at the current
  node. Set on `Submitted + CoderStarted` (entry to `Walking`) and
  on rework re-spawn (`Walking + CoderStarted` at the entry node).
- `pr_tracking { pr_number, head_sha }` — set by the shipper's
  `RoleDone` payload; the walker propagates it forward across the
  ci-watcher node so rebase plumbing can update `head_sha` via
  `Rebased { new_head_sha }`.
- `operator_decision { disagreements }` — coder reported a
  deliberate disagreement (`self_review_disagreed`) and the brief
  is parked waiting for `CaptainAccepted` or `CaptainRejected`. This
  is the post-collapse home of what the legacy FSM called the
  `AwaitingCaptainDecision` BriefState variant — it's a RunData
  discriminant now, not a top-level state.
- `extension { data: serde_json::Value }` — free-form JSON escape
  hatch for downstream-extension nodes. `Eq` is intentionally not
  derived on `RunData` because the `Extension` variant carries
  `serde_json::Value`, which blocks structural equality.

Helper accessors on `RunData` (`agent_id`, `pr_number`, `head_sha`,
`disagreements`) yield `Option<&T>` for the relevant variant so call
sites stay type-safe without matching exhaustively.

- depends on: DisagreementSummary

## BriefEvent

The discrete inputs the FSM accepts. Tagged `kind` + snake_case.
Each variant carries the minimal data needed to compute the next
state.

The events split into three groups:

**Specialised events** that the FSM still routes through dedicated
arms in `handle()`:

- `CoderStarted { agent_id, role_name, started_at }` — coder
  container spawned. From `Submitted` it transitions the brief into
  `Walking` at the entry node (`NodeId(role_name)`) with
  `RunData::Coder { agent_id }`; from `Walking` at the entry node
  it is the rework re-spawn (reset evidence + run_data, preserve
  retry).
- `CoderDoneNoOp { reason }` — coder reported terminal `Shipped`
  but produced no diff against base (acceptance passed against work
  already on the base branch). Short-circuits `Walking{coder} →
  Shipped`, bypassing every downstream node. Only legal at the
  entry coder node.
- `CoderDisagreed { disagreements }` — coder's self-review found
  unapplied verbs but every miss carries `applied_form` +
  `rationale` (deliberate disagreement, not failure). Flips the
  coder's `Walking{run_data: Coder{..}}` to
  `Walking{run_data: OperatorDecision{disagreements}}`; `node_id`,
  `evidence`, and `retry` are preserved. The brief is parked until
  `CaptainAccepted` / `CaptainRejected` fires.
- `CiResult { state: CiState, head_sha }` — CI watcher reports
  success / failure / pending. Routed through its own arm in
  `handle()` because the rebase/CI loop has a distinct retry
  policy (failure rewinds to the entry node and bumps `retry`;
  pending stays).
- `RebaseStarted` — no-op stay (observability marker for the
  state log).
- `Rebased { new_head_sha }` — when the current `run_data` is
  `PrTracking`, updates `head_sha`; otherwise no-op.
- `CaptainAccepted` — operator endorses the disagreed-form output.
  Treats the coder's contribution as `EventVerdict::Shipped` in
  `evidence` and calls `advance_walker` so the brief continues
  downstream as if the coder had shipped normally.
- `CaptainRejected { reason }` — operator rejects. Fails the brief
  with `Reason::CaptainRejectedDisagreement { reason }`.
- `PreflightSmellDetected { smell_id, criterion, baseline }` —
  preflight-criterion-agentry detected a blocking smell heuristic
  on the brief's `success_criteria`. Only legal at the entry coder
  node; fails the brief with `Reason::PreflightSmell`.

**Universal events** that are handled at the top of `handle()`
against every non-terminal state:

- `AbortRequested { actor, message }` — fails the brief with
  `Reason::AbortRequested { actor, message }` from any non-terminal
  state.
- `BudgetExhausted` — fails the brief with
  `Reason::BudgetExhausted` from any non-terminal state. Emitted
  by the wall-clock reaper and by `increment_or_fail` callers.

**The generic per-node completion event**:

- `RoleDone { node_id, verdict, findings, run_data }` — the single
  generic role-completion event after the collapse. The translator
  emits this for every `EventKind::Done` regardless of role family
  (coder, ac-verifier, reviewer, shipper, ci-watcher). Carries the
  reporter node id, its verdict, any review findings the role
  produced, and an optional `RunData` payload. The shipper's emit
  carries `Some(RunData::PrTracking { pr_number, head_sha })` so
  the walker's next state (the ci-watcher node) inherits PR
  identifiers; other roles emit `run_data: None`.

The coder's `self_review_disagreed` cause path now flows through
`CoderDisagreed → Walking { run_data: OperatorDecision }`. Pre-collapse
this routed through `Authoring → AwaitingCaptainDecision`.

`RetryRequested { actor, reason }` is handled on the terminal
`Failed` state and transitions back to `Submitted` (operator-driven
retry of a failed brief, re-running the whole walk).

## Reason

Why a brief landed in `BriefState::Failed`. Tagged enum (discriminator
`kind`). Variants:

- `BudgetExhausted` — retry counter exceeded `RetryBudget.max`, or
  the daemon raised `BudgetExhausted` from a token / wall-clock cap.
- `AbortRequested { actor, message }` — a human or supervising
  agent issued an abort.
- `AcceptanceFailed { detail }` — a gate (per `WalkConfig` per-node
  `policy`) returned a non-rework hard failure verdict
  (`Rejected` / `Escalated`). Details carry the reporting node and
  the verdict family.
- `PreflightSmell` — `preflight-criterion-agentry`'s smell
  heuristics fired on the brief's `success_criteria`; see below.
- `DaemonError { detail }` — an internal substrate failure. The
  lifecycle driver also synthesises this on `InvalidTransition`
  (every (state, event) pair either transitions or fails the brief
  — silent drops are forbidden).
- `DaemonRestartedDuringExecution` — the daemon's boot-time resume
  scan found a brief in a non-terminal `:state` whose container is
  gone (or whose reattach setup failed).
- `CaptainRejectedDisagreement { reason }` — captain explicitly
  rejected a coder-flagged disagreement via `captain decide reject`.
  The reason carries the captain's prose explanation.
- `TopologyInvalid { detail }` — **NEW in beta-b.** Fires when
  `lifecycle_driver::derive_entry_node` cannot identify a unique
  topology root (zero or more than one node has empty
  `expected_inbound`). A topology-data bug — the brief fails with
  this so the operator fixes the topology rather than the daemon
  silently routing through an arbitrary node.

`PreflightSmell` fires when preflight-criterion-agentry detects that
the operator-authored `success_criteria` matches one of the blocking
smell heuristics (a count-zero `wc -l` filter against a baseline >100;
a literal `grep -v 'mod tests'` filter that does not exclude
`#[cfg(test)]` scopes — see the runner for the current rule set). It
is distinct from `AcceptanceFailed` along an authoring-vs-execution
axis: `AcceptanceFailed` means the work itself missed the bar
(coder/ac-verifier/reviewer rejected the diff); `PreflightSmell`
means the bar itself is malformed before any work runs. Per the
brief-84b grill-me transcript (Q4), there is no operator-override
mechanism — the smell heuristics ARE the contract, and refining them
is a code-level PR against the runner. The operator's response is to
rewrite the criterion to be more specific (e.g. swap `wc -l` for a
Rust-aware tool like `ra-query` or `cfdb`) and resubmit. Smell details
(which smell-id fired, criterion text, baseline value) ride in the
`BriefEvent::PreflightSmellDetected` payload that triggers the
transition; the variant itself carries no payload so dashboards
surface a typed badge per smell-class without re-parsing prose.

## GatePolicy

The rule applied at a node fan-in to fold a multiset of inbound
verdicts into a single `Decide` outcome. Three variants:

- `AllMustPass` — every entry in `expected_roles` must report
  `Shipped`; any other verdict triggers `Rework` (soft fails:
  `Failed` / `ReworkNeeded`) or `Reject` (hard fails: `Rejected` /
  `Escalated`).
- `FailFast` — same short-circuit verdicts but evaluated as
  evidence arrives, without waiting for siblings; the first
  non-`Shipped` verdict transitions the brief immediately.
- `Majority { threshold_pct }` — the `Shipped` count must reach
  the threshold percentage of expected roles to `Pass`; soft fails
  after the threshold is unreachable trigger `Rework`; hard fails
  always `Reject`.

## GateConfig

Pairs a `GatePolicy` with the list of role-names the gate waits on.
`expected_roles` enumerates the role-name strings (the `.0` of each
`NodeId` in the per-node `expected_inbound`) that must appear in the
evidence multiset for the gate to reach a terminal `Pass` outcome.
The shape is generic — `agentry-self-host-v0`'s verifier fan-in
carries three `ac-verifier-*` names, but `GateConfig` accepts any
list. Constructed on the fly by `advance_walker` from each downstream
node's `NodeConfig` (it projects the per-node `expected_inbound: Vec<NodeId>`
into `expected_roles: Vec<String>`).

- depends on: GatePolicy

## Decide

The return value of the pure `decide` function that folds a node's
collected verdicts against its `GateConfig`. Four variants: `Wait`
(collect more evidence), `Pass` (gate satisfied — advance to this
downstream), `Rework { detail }` (soft failure — rewind to the
entry node and bump retry), `Reject { detail }` (hard failure —
fail the brief with `Reason::AcceptanceFailed`). `Decide` is a
transient return value; it is not persisted, not serialized, and
does not appear in `BriefStateRecord`.

## ReworkTarget

Pre-collapse: enum signalling which role re-runs when the FSM enters
`Reworking` (`Coder` re-spawns; `Reviewer` re-runs the deterministic
fences against the unchanged diff). Post-collapse the rework loop is
implicit in `WalkConfig.adjacency` — soft-fail outcomes rewind the
walker to the entry node and bump `retry`, so the topology root is
the only rework target. The enum is retained in
`crates/orchestrator-types/src/lifecycle.rs` as a no-longer-consumed
type pending follow-up cleanup; nothing in `handle()` reads it.

## CiState

The CI status carried by a `BriefEvent::CiResult`. Three variants —
`Pending`, `Success`, `Failed` — matching the gitea poller's coarse
view. `Success` transitions a `Walking` brief to terminal `Shipped`;
`Failed` rewinds to the entry node and bumps `retry` (or
short-circuits to `BudgetExhausted` when the bump would breach the
cap); `Pending` is a no-op that keeps the brief at its current
`Walking` state so the projector still records the poll for
observability.

## NodeConfig

Per-node walker config carrying the node's `NodeClass`, the
deduplicated upstream `NodeId`s that must report before the node is
considered ready (`expected_inbound`), and the `GatePolicy` used to
fold inbound verdicts. Loaded-bearing post-collapse: the walker reads
this on every advance step to construct the per-node `GateConfig`
fed to `decide()`.

- depends on: NodeClass
- depends on: NodeId
- depends on: GatePolicy

## WalkConfig

Adjacency map plus per-node configs for the lifecycle DAG walker.
Built from `TeamTopology` by the runtime helper `build_walk_config`
(in `crates/orchestrator-runtime/src/lifecycle_driver.rs`), which
groups `MessageEdge`s by `from` to populate adjacency and walks
`team.roles` cross-referenced against `team.node_classes` to populate
`node_configs`. The per-edge `GatePolicy` (`MessageEdge.gate_policy`)
overrides the default `AllMustPass` when the topology declares one.

Load-bearing post-collapse: every `handle()` call consumes a
`&WalkConfig`. The projector builds it once per brief at dispatch
(or reattach) time and threads it on every step.

`derive_entry_node(walk_config) -> Result<NodeId, Reason>` computes
the topology root — the unique node whose `expected_inbound` is
empty. Zero roots or more than one are surfaced as
`Reason::TopologyInvalid` so the brief fails cleanly rather than
routing through an arbitrary node.

- depends on: NodeId
- depends on: NodeConfig

## InvalidTransition

Returned by `handle` when an event is not legal in the current state.
Carries an owned snapshot of both the offending state and the event
that triggered the rejection so the daemon can log the pair without
re-borrowing the originals. Marked `Clone + PartialEq` so tests can
compare the rejection shape and the daemon can attach the value to a
trace event. Boxed at the `Result` boundary because `BriefState +
BriefEvent` cross clippy's `result_large_err` threshold.

## BriefInventory

Read port for the wall-clock reaper. Yields every in-flight brief's
latest `BriefStateRecord` plus that brief's declared
`budget.max_wall_seconds`. Production scans `agentry:brief:*:state`;
tests inject deterministic fixtures. Decoupled from the
`StateProjector` write port so the reaper does not need a per-brief
projector instance to enumerate orphans.

## ReaperSink

Write port for the wall-clock reaper. Two effects: `push_event`
appends a `BriefEvent` to the brief's trace stream so the per-brief
lifecycle FSM driver picks it up and transitions, and
`kill_containers` shells out to `podman kill` against the
`agentry.brief={id}` label so the orphan stops burning tokens. The
trace push is correctness-critical (drives the FSM), the container
kill is best-effort.

## RedisInventory

Production `BriefInventory` adapter. SCAN-pages
`agentry:brief:*:state`, GETs each match, and parses the JSON
`BriefStateRecord`. Body lookups GET `agentry:brief:{id}:body` and
walk the JSON to `payload.budget.max_wall_seconds` — bypassing
`serde_json::from_str::<Brief>` keeps the reaper insensitive to
future Brief-shape changes outside the budget field.

## RedisReaperSink

Production `ReaperSink` adapter. Pushes lifecycle events to the trace
stream as `EventKind::Event { payload: <BriefEvent JSON> }` — the
`RedisEventSource` translator recognises this shape and yields the
carried `BriefEvent` to the per-brief FSM driver. `podman kill`
shells out via `tokio::process::Command`.

#### Operational invariants (not enforced by graph-specs)

- FSM rejects no events silently. The lifecycle driver translates
  `InvalidTransition` into `BriefState::Failed { reason: DaemonError }`
  instead of WARN-and-skip. WHY: silent drops were the root cause of
  #396 — the 2nd and 3rd reports of an ensemble verifier were lost
  when the FSM model assumed serial transitions but the runtime fanned
  out roles in parallel. The structural fence is: every (state, event)
  pair either transitions or fails the brief; there is no third path.

- **Late-event fence.** A `RoleDone` for a node that is strictly
  upstream of the walker's current `node_id` (in the
  `WalkConfig.adjacency` forward direction) is a "late event". The
  FSM returns the state unchanged on these; the lifecycle driver
  detects the no-op transition via `is_late_event` and emits a
  `tracing::warn` rather than propagating an `InvalidTransition`.
  Replaces the legacy "first matching event wins" silent-drop
  behaviour with explicit observability.

- **Topology root is derived, not configured.** The unique
  entry-vertex `NodeId` is computed once by
  `lifecycle_driver::derive_entry_node` from `WalkConfig` at
  dispatch/reattach time and threaded into every `handle()` call.
  Topologies with zero or multiple roots fail the brief with
  `Reason::TopologyInvalid` — there is no "first node by hash order"
  fallback.

- **Empty-downstream sink.** A node with no entries in
  `WalkConfig.adjacency` is a terminal sink: when a `RoleDone` for
  it folds into `evidence` and `advance_walker` finds no
  downstreams, the brief transitions to terminal `Shipped`. This
  replaces the per-phase auto-skip that the legacy FSM used to
  bridge leaner topologies (e.g. `agentry-bugfix-v0` with no
  verifier); now leaner topologies just have fewer DAG nodes, and
  the walker reaches the sink correctly without any phase-enum-
  specific short-circuit.

#### Wall-clock reaper transition (not enforced by graph-specs)

The daemon's wall-clock reaper closes the orphan-without-Failed class
documented in `docs/forensics/orphan_pattern.md` (Cases 2/3/4 — PRs
#374, #381, #382 from session 2026-05-04, where containers died
silently before emitting their terminal `Done` event).

Every 30 seconds the reaper sweep runs against every
`agentry:brief:{id}:state` record. For each non-terminal record (any
`BriefState` variant other than `Shipped` or `Failed`) whose `now() -
record.at` exceeds the brief's `budget.max_wall_seconds` (with a
30-minute daemon-level fallback when absent), the reaper pushes
`BriefEvent::BudgetExhausted` into the brief's trace stream and
best-effort `podman kill`s any container labeled
`agentry.brief={id}`. The push lands as
`EventKind::Event { payload }` carrying the serialised `BriefEvent`;
`RedisEventSource::next` translates it back to the typed event so the
FSM driver applies the existing universal-aborts arm of `handle()`,
which transitions the brief to `BriefState::Failed { reason:
BudgetExhausted }` from any non-terminal state.

Boundary semantics for the reaper's `is_orphan` predicate are strict
greater-than: a record exactly at the budget is NOT yet orphan (avoids
double-fire on a freshly-stamped boundary), one second over is.
Terminal states are never orphan regardless of elapsed time; clock
skew that places `record.at` in the future is also not-orphan.

#### Role-name → kind mapping (not enforced by graph-specs)

The spawner emits the full role name on each agent's spawn event
(`role_name: "coder-claude-agentry"`, `"shipper-agentry"`, …). The
translator's `role_by_agent` memo must store the SHORT kind that the
`Done`-branch match arms compare against — otherwise the lookup
returns the full name, no arm fires, and the FSM never advances out
of the entry node. The mapping table:

| Spawned role-name shape           | Short kind     | Done-branch BriefEvent                              |
|-----------------------------------|----------------|-----------------------------------------------------|
| `coder-*`                         | `coder`        | `CoderStarted` (on spawn), `RoleDone` (on done)     |
| `ac-verifier-*`                   | `ac-verifier`  | `RoleDone`                                          |
| `verifier-*`                      | `verifier`     | `RoleDone`                                          |
| `reviewer-*`                      | `reviewer`     | `RoleDone { findings: [...] }`                      |
| `shipper-agentry` (exact)         | `shipper`      | `RoleDone { run_data: Some(PrTracking{..}) }`       |
| `ci-watcher-agentry` (exact)      | `ci-watcher`   | `CiResult { state: <verdict→CiState> }`             |
| `preflight-criterion-*`           | `preflight`    | (none — preflight emits its own typed BriefEvent)   |

CI-watcher verdict mapping: `EventVerdict::Shipped → CiState::Success`,
`EventVerdict::Failed → CiState::Failed`, all other verdicts (`Escalated`,
`ReworkNeeded`, `Rejected`) → `CiState::Pending`. The watcher does not
normally emit the latter three on its happy path; the Pending fallback
keeps the brief at the ci-watcher `Walking` node so the next poll tick
can advance it.

Unrecognised role names are NOT memoized — the `Done` lookup falls
through to the catch-all (no `BriefEvent` emitted), preserving the
"unknown role is invisible" invariant rather than silently
mis-classifying a future role family.

#### FSM transition flow (not enforced by graph-specs)

After the collapse the FSM is no longer a fixed phase chain. The
shape of the walk is whatever the brief's `WalkConfig` adjacency
declares. The illustrative `agentry-self-host-v0` happy path:

```
Submitted
  → Walking{coder, run_data: Coder}          (CoderStarted)
  → Walking{verifier_n, run_data: None}      (RoleDone Shipped from coder, AllMustPass on ac-verifier-*)
  → Walking{reviewer_n, run_data: None}      (RoleDone Shipped from each ac-verifier, gate Pass)
  → Walking{shipper, run_data: PrTracking}   (RoleDone Shipped from reviewer-*, gate Pass — shipper's RoleDone carries PrTracking)
  → Walking{ci-watcher, run_data: PrTracking}(advance_walker forwards PrTracking)
  → Shipped                                  (CiResult Success)
```

`Walking{ci-watcher}` is a self-loop on `CiResult Pending`.
`Walking{ci-watcher} + CiResult Failed` rewinds to the entry coder
node and bumps `retry` (or short-circuits to `BudgetExhausted`).
`Walking{coder, run_data: Coder} + CoderDoneNoOp` short-circuits
straight to terminal `Shipped`.

The "phases" the legacy FSM hard-coded (`Authoring`, `Verifying`,
`Reviewing`, `Shipping`, `Watching`) survive only as role-name
prefixes inside `WalkConfig.node_configs` keys. A topology that
declares no `verifier-*` role simply has no verifier nodes in its
adjacency, and the walker advances coder → reviewer directly.

#### F6 escalation — coder disagreement → captain decide

When the coder's self-review reports `all_applied=false` but every
miss carries `applied_form` + `rationale` (deliberate disagreement,
not failure), the daemon's trace translator detects the
`cause = "self_review_disagreed"` payload on the coder's `Done`
event and emits `BriefEvent::CoderDisagreed { disagreements }`.

`handle()`'s CoderDisagreed arm flips the coder node's `run_data`:

```text
Walking { node_id: coder_node,
          evidence,
          run_data: RunData::Coder { agent_id },
          retry }
    + CoderDisagreed { disagreements }
=>
Walking { node_id: coder_node,
          evidence,                      // preserved
          run_data: RunData::OperatorDecision { disagreements },
          retry }                        // preserved
```

The brief is now parked indefinitely — no timeout, no automatic
retry — waiting for an explicit captain decision. `node_id`,
`evidence`, and `retry` are all preserved so when the captain
accepts, the walk can resume from where the coder left off.

`captain decide accept <brief_id>` pushes `BriefEvent::CaptainAccepted`
onto the brief's trace stream; the FSM's CaptainAccepted arm
records the coder's contribution as `EventVerdict::Shipped` in
`evidence` and calls `advance_walker` so the brief proceeds to the
post-coder downstream nodes using the work already in the brief
workspace, exactly as if the coder had emitted the literal verb.
The `run_data` resets to `None` on the next node (the agent_id is
no longer known — the operator-decision park is post-coder-exit).

`captain decide reject <brief_id> --reason '...'` pushes
`BriefEvent::CaptainRejected { reason }`; the FSM fails the brief
with `Reason::CaptainRejectedDisagreement { reason }`.

`captain decide list` shows currently parked briefs as one JSON
object per line, `{"brief_id":"…","disagreements":N,"parked_at":"…"}`.
The captain CLI identifies parked briefs by scanning `:state` records
for the `Walking{run_data: OperatorDecision{..}}` shape — the
post-collapse home of what was previously the
`BriefState::AwaitingCaptainDecision` variant.

All three subcommands push their event onto
`agentry:brief:<id>:trace` with `agent` set to `captain-cli`, where
the per-brief lifecycle driver consumes it like any other FSM event.

The dashboard surfaces parked briefs in the standard in-flight list
with a prominent AWAITING CAPTAIN DECISION badge and the
disagreements rendered inline (verb, what the coder did instead,
rationale). The badge derives from the `run_data: OperatorDecision`
shape, not from a dedicated BriefState variant.

#### Operator abort (not enforced by graph-specs)

`orchestrator abort <brief_id>` is the canonical surgical-shutdown path.
The CLI looks up the brief's `agentry:brief:{id}:state` record and, when
the state is non-terminal, pushes a
`BriefEvent::AbortRequested { actor: "operator", message: "orchestrator
abort cli" }` onto the brief's `:trace` stream as
`EventKind::Event { payload: <BriefEvent JSON> }`. The per-brief
lifecycle driver consumes the event through `RedisEventSource`, the
universal aborts arm of `handle()` transitions any non-terminal
`BriefState` to `Failed { AbortRequested }`, and the projector writes
the resulting `BriefStateRecord` to `:state` and `:state_log`. On a
terminal `:state` the CLI is idempotent: it prints
`status: "already_terminal"` and exits cleanly without re-pushing.
A missing `:state` key returns `no such brief: {id}` with exit code 1.
The legacy `orchestrator abort --all` path (kill every
`agentry.brief`-labelled container) is preserved byte-for-byte for
operator muscle memory.

`--keep-workspace` preserves the brief's workspace dir under
`<root>/briefs/{brief_id}/` for forensics; without the flag the abort
path `rm -rf`s the dir after the FSM transition. `--keep-trace`
preserves the `:trace` stream and `:state_log`; without the flag the
CLI sleeps briefly so the per-brief driver consumes `AbortRequested`
and writes the terminal `:state`, then `XTRIM`s `:trace` to `MAXLEN 0`
and `DEL`s `:state_log` — the verdict has already been recorded in
`agentry:verdicts` at that point and the trace stream is no longer
load-bearing for the FSM. NOTE: the per-brief abort assumes a live
driver; if the daemon restarted pre-471b reattach, the
`AbortRequested` event sits in `:trace` unconsumed. The operator
should fall back to the future #471b reattach OR a direct redis
`:state` patch as the hard escape.

## ResumeReport

#### Daemon resume on boot

Every `orchestratord` boot runs an orphan scan before the wall-clock
reaper starts: `crate::daemon_resume::resume_orphans` SCANs every
`agentry:brief:*:state` key, deserializes each as a
`BriefStateRecord`, and for every record still in a non-terminal
position checks whether the named container (`agentry-{agent_id}`) is
still alive via `podman ps`. The scan is non-blocking SCAN, never
`KEYS`, so the cost is bounded on a populated production Redis.

Each non-terminal record lands in exactly one of three buckets, all
exposed on `ResumeReport`:

- **Live container, reattach OK** → `kept_alive`. The daemon re-spawns
  `lifecycle_driver::projector_task` for the brief: it reads the brief
  body from `agentry:brief:{id}:body`, fetches the team topology via
  `redis_io::fetch_team`, builds the `WalkConfig` with
  `lifecycle_driver::build_walk_config`, derives the entry node with
  `lifecycle_driver::derive_entry_node`, and hands the resulting
  `EventSource` / `StateProjector` (constructed via the same factories
  the original dispatch used) to a fresh `projector_task`. The
  projector resumes reading the brief's trace stream and observes any
  subsequent terminal event the agent container produces. The `:state`
  record is left untouched — the brief is genuinely still in flight.
- **Live container, reattach setup failed** → `reattach_failed`. The
  body GET, body deserialization, team fetch, `WalkConfig` build, or
  entry-node derivation failed (most commonly: the body key was
  evicted or never written; or the topology was malformed and
  `derive_entry_node` returned `Reason::TopologyInvalid`). The brief
  is marked `Failed { DaemonRestartedDuringExecution }` exactly as
  if the container were dead, so the FSM lands cleanly terminal and
  the operator can resubmit. The container itself is intentionally
  NOT killed — operator may want to inspect.
- **Dead container** → `failed_dead`. The named container is gone (or
  the record's state has no `agent_id` to probe). The resume path
  writes a fresh
  `BriefStateRecord { state: Failed { reason: DaemonRestartedDuringExecution } }`
  back to the `:state` key and appends it to `:state_log`, so the FSM
  lands in a consistent terminal state and the operator can resubmit.

Terminal records (`Shipped`, `Failed`) are skipped silently and are
NOT counted in `scanned`, so re-running the scan is idempotent and
never re-writes or duplicates `state_log` entries for already-finished
briefs. The invariant `scanned == kept_alive + failed_dead +
reattach_failed` holds for the records the scan touched.

**Reattach limitation (v0).** The reattach path only re-spawns the
per-brief lifecycle driver — the projector that consumes the trace
stream and writes the terminal verdict. It does NOT re-spawn the
daemon's role chain (the in-process `handle_brief` outbox-watching
loop). The role chain's transient state — polling cursors, in-memory
retry budgets, semaphores — is gone at restart and is not
reconstructed here. Concretely: a reattached brief whose coder is
still running WILL ship the coder's work and emit the terminal verdict
via the projector; it will NOT auto-progress to the next role in the
chain (e.g., the reviewer is not spawned after the coder ships
post-reattach). A v2 reattach can re-spawn the role chain via a
follow-up brief; for v0 this is the acceptable cost for not losing all
in-flight work on every daemon redeploy.

**Trace cursor on reattach.** The new `RedisEventSource` constructed
by the reattach factory begins reading at `"0-0"` (the spec invariant
matches the original-dispatch cursor). The projector therefore replays
every past trace event for the brief through `handle()`. FSM
transitions are pure so the final state is deterministic, but the
projector's `:state_log` XADDs are NOT idempotent — replay produces
duplicate `:state_log` entries for transitions that already landed
pre-restart. The terminal verdict is only emitted when the FSM
actually reaches a terminal state, so it is NOT double-emitted at
reattach time for in-flight briefs (their pre-restart events are not
terminal by definition). This is the v0 cost of the cursor-from-zero
reattach; a future slice can land a real
`agentry:brief:{id}:state_projector_cursor` cursor in the trace-stream
id space (the existing `:state_projector_cursor` key today carries a
synthetic `step-N` counter, not a redis stream id, and so cannot be
fed to `XREAD` directly).

**Race: container exits between scan and projector spawn.** The boot
scan runs `podman ps` then schedules the `projector_task` spawn; the
container can exit in the gap. If the container produced its terminal
trace event before exiting, the projector observes it on first
`XREAD` and writes the verdict — same happy-path code as a normal
shipping brief. If the container exited without emitting a terminal
event (e.g., crashed mid-execution), the projector blocks on `XREAD`
until the wall-clock reaper fires `BriefEvent::BudgetExhausted` into
the trace stream, at which point the FSM's universal-aborts arm
transitions the brief to `Failed{BudgetExhausted}` and the projector
emits the verdict and exits. So the worst-case behaviour is "projector
waits up to one wall-clock budget interval before terminating," not
"projector loops forever."

The pre-471a behaviour — the FSM staying in a non-terminal state
forever and the dashboard showing phantom "running" briefs
indefinitely — is closed by the failed-bucket paths. The pre-471b
behaviour — every operator-initiated daemon redeploy killing all
in-flight work even when the containers were still running — is
closed by the reattach (kept_alive) path. If the operator needs
orphaned work redone, they can resubmit with the same brief id; the
new run will be a fresh trip through the FSM.

Briefs whose `Walking.run_data` is `OperatorDecision{..}` (the
post-collapse home of the legacy `AwaitingCaptainDecision` state)
also reattach on boot — `projector_task` is re-spawned to consume
the eventual `CaptainAccepted` or `CaptainRejected` event the
operator will push via captain decide CLI. No agent-container check
is needed for this shape; the brief is operator-gated, not
container-gated. Other non-entry-coder `Walking` records (verifier,
reviewer, shipper, ci-watcher nodes) continue to be marked
`Failed{DaemonRestartedDuringExecution}` on boot — closing that gap
requires the role-chain re-spawn story which lives in a future brief.

## DisagreementSummary

One coder-flagged disagreement with a brief verb. The struct carries
`verb` (the literal verb the coder did not apply), `applied_form`
(the variant the coder emitted instead), and `rationale` (the coder's
reason for the substitution). F6a (PR #443) added these fields to the
role-runtime `UnappliedVerb` shape; F6b (this brief + 449b) lifted
them into orchestrator-types so the FSM can carry disagreements
without a role-runtime dependency. Wire-equivalent to
`UnappliedVerb` at the JSON level; `serde(deny_unknown_fields)` so
extra keys are a hard error rather than silently dropped.

Post-collapse, `DisagreementSummary` rides on the
`BriefEvent::CoderDisagreed` payload and is then carried in
`BriefState::Walking { run_data: RunData::OperatorDecision
{ disagreements } }` until the captain decides. The flow is
captain-mediated: when the coder's self-review reports
`all_applied=false` but every miss carries `applied_form` +
`rationale` (deliberate disagreement, not failure), the FSM transitions
from `Walking { run_data: Coder { .. } }` to `Walking { run_data:
OperatorDecision { disagreements } }` with `node_id`, `evidence`, and
`retry` preserved. The brief is parked indefinitely — no timeout, no
automatic retry — waiting for an explicit captain decision. The
preserved retry budget means semantics are preserved if the captain
rejects and the operator resubmits.

`CaptainAccepted` treats the disagreed-form output as morally
equivalent to `RoleDone { verdict: Shipped }` from the coder node.
The brief proceeds through the downstream nodes using the work
already in the brief workspace, exactly as if the coder had emitted
the literal verb. Topologies with no downstreams (or whose walker
reaches a sink) terminate cleanly at `Shipped` just like the regular
coder-shipped path.

`CaptainRejected` fails the brief with reason
`CaptainRejectedDisagreement` carrying the captain's prose
explanation. The operator can resubmit a fresh brief id with a
corrected verb after seeing the captain's reason on the terminal
verdict.

#### Migration appendix — beta-b collapse (post-#495b)

The 11-variant phase-enum FSM was collapsed to 4 variants in
#495 beta-b. The mapping from the pre-collapse vocabulary to the
post-collapse vocabulary:

| Pre-collapse                                       | Post-collapse                                                                 |
|----------------------------------------------------|-------------------------------------------------------------------------------|
| `BriefState::Authoring { agent_id, started_at, .. }` | `BriefState::Walking { node_id: coder_node, run_data: Coder { agent_id }, .. }` |
| `BriefState::Verifying { received, expected, .. }` | `BriefState::Walking { node_id: ac_verifier_n, evidence, .. }`                |
| `BriefState::Reviewing { received, expected, .. }` | `BriefState::Walking { node_id: reviewer_n, evidence, .. }`                   |
| `BriefState::Reworking { .. }`                     | `BriefState::Walking { node_id: <entry coder>, retry: bumped, .. }`           |
| `BriefState::Shipping { pr_number, head_sha, .. }` | `BriefState::Walking { node_id: shipper_node, run_data: PrTracking, .. }`     |
| `BriefState::Watching { pr_number, head_sha, .. }` | `BriefState::Walking { node_id: ci_watcher_node, run_data: PrTracking, .. }`  |
| `BriefState::Extension { .. }`                     | `BriefState::Walking { node_id: <topology-declared>, run_data: Extension, .. }` |
| `BriefState::AwaitingCaptainDecision { .. }`       | `BriefState::Walking { run_data: OperatorDecision { .. }, .. }`               |
| `BriefEvent::CoderDone { verdict }`                | `BriefEvent::RoleDone { node_id: coder_node, verdict, .. }`                   |
| `BriefEvent::AcVerifierDone { role_name, verdict }`| `BriefEvent::RoleDone { node_id, verdict, .. }`                               |
| `BriefEvent::ReviewerDone { role_name, .. }`       | `BriefEvent::RoleDone { node_id, verdict, findings, .. }`                     |
| `BriefEvent::ShipperDone { pr_number, head_sha }`  | `BriefEvent::RoleDone { node_id, run_data: Some(PrTracking{..}), .. }`        |
| `PhaseGates { verifying: GateConfig, reviewing: GateConfig }` | `WalkConfig.node_configs[node_id].policy / .expected_inbound` (per-node) |
| `ReworkTarget` enum                                | Implicit in `WalkConfig.adjacency` (rework rewinds to entry node)             |
| `lifecycle::role_kind(role)` lookup                | Still present — translator memoizes the short kind; the FSM keys on `NodeId(role_name)` directly |

Pre-beta-b briefs in the legacy `BriefState` shape will fail to
deserialize after this lands (the persisted JSON variant tags
`authoring` / `verifying` / etc. are gone). The migration brief
**#497** drained in-flight briefs before the cutover; see
`docs/captain-doctrine.md` for the redeploy + drain protocol.

`PhaseGates` was deleted in this collapse; per-node gate config now
lives on `WalkConfig.node_configs[n].policy` and
`WalkConfig.node_configs[n].expected_inbound`. The runtime helper
`build_walk_config` populates this from `TeamTopology.message_graph`
+ `TeamTopology.roles` + `TeamTopology.node_classes` +
`MessageEdge.gate_policy`.

The `lifecycle::role_kind` helper is still in
`crates/orchestrator-types/src/lifecycle.rs` — the translator uses
it to map a spawn event's full role name to the short kind for the
`Done`-branch lookup, but the FSM itself no longer keys on the short
kind; it keys on `NodeId(role_name)` (the full role name).
