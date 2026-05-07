# Brief lifecycle

> Status: **ratified**. Code landing PR: L.1 (EPIC #246). The pure FSM
> types and `handle` transition function live in
> `crates/orchestrator-types/src/lifecycle.rs`. Daemon wiring,
> projection on Redis streams, and the persisted state-stream substrate
> land in L.2 under `specs/concepts/brief_state_stream.md`.

The bounded context that owns *the position of a brief inside its
end-to-end shipping pipeline*. A brief is a single record that travels
from submission, through coding, acceptance verification, review,
shipping, CI watch, and finally to one of two terminals. The lifecycle
captures only what is *observable about the brief itself* — agent
identities, retry counters, the PR number once one exists, the head
SHA being watched. Per-agent execution detail (token usage, transcript
paths, watchdog signals) is not part of this concept; it lives in the
agent contract and event streams.

The FSM is a pure transition table: `handle(state, event) -> new
state`. Wall-clock time, persistence, agent dispatch, and human
notification are layered by the daemon caller. Keeping the table free
of I/O is what makes the entire flow replayable: the same event log
projected against the same starting state will always produce the
same sequence of `BriefStateRecord`s. The L.2 brief introduces an
`EventSource` and `StateProjector` that consume this FSM.

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

The position a brief occupies in the lifecycle. Sum type with ten
variants: one entry state (`Submitted`), seven non-terminal working
states (`Authoring`, `Verifying`, `Reviewing`, `Reworking`,
`Shipping`, `Watching`, `Extension`), and two terminals (`Shipped`,
`Failed`). Non-terminal variants carry their `RetryBudget`; the two
terminals carry only the outcome. `Authoring` additionally pins the
`agent_id` that started the run and the `started_at` timestamp; the
shipping/watching states carry the gitea `pr_number` and the `head_sha`
CI is being polled against. `Extension` is a forward-compat slot for
team-specific intermediate states the core FSM does not need to know
about (e.g. a planner's child-fanout phase) — only the universal
aborts apply to it; all other events return `InvalidTransition`.

`Verifying` and `Reviewing` additionally carry three evidence-shape
fields landed in 396b-2: `received` (a `BTreeMap<String, EventVerdict>`
of `role_name` → verdict, accumulating sibling reports as they arrive),
`expected` (the list of `role_name` strings the gate waits on,
populated from the team topology at phase entry by
`build_phase_gates`), and `policy` (a `GatePolicy` variant indicating
the fan-in rule). Per 396b-3 `handle()` transitions out of the phase
only when the gate's policy declares Pass over the accumulated
`received` multiset; transitions to Reworking on soft fails
(Failed/ReworkNeeded), to Failed{AcceptanceFailed} on hard fails
(Rejected/Escalated). Each new sibling verdict is folded into
`received` and `handle()` calls
`decide(received, &GateConfig{expected, policy})` to pick Wait /
Pass / Rework / Reject — the FSM is now parallel-aware and no
sibling's report is silently dropped.

## BriefEvent

The discrete inputs the FSM accepts. One variant per externally
observable signal: agent-emitted (`CoderStarted`, `CoderDone`,
`AcVerifierDone`, `ReviewerDone`, `ShipperDone`), gitea-poller-emitted
(`CiResult`, `RebaseStarted`, `Rebased`), and human/operator-emitted
(`RetryRequested`, `AbortRequested`). `BudgetExhausted` is a
self-emitted event the daemon raises when external bookkeeping (token
spend, wall-clock) crosses a `Budget` cap; the FSM treats it as just
another input so the budget enforcement layer stays decoupled. Each
variant carries the minimal data needed to compute the next state —
verdicts, PR numbers, head SHAs, actor identities.

`AcVerifierDone` and `ReviewerDone` additionally carry the originating
`role_name` (the full role name like `"ac-verifier-claude-agentry"` or
`"reviewer-mechanical-agentry"`) so the planned evidence multiset
(396b-2) can key by sibling and distinguish reports from a fan-out (e.g.
the three `ac-verifier-*` roles or the two `reviewer-*` roles under
`agentry-self-host-v0`). Other variants are unchanged. The `role_name`
field is populated by `translate_trace_entry` in the runtime adapter at
the projector boundary — the same site that already maps each agent's
spawned role onto an `EventKind`-shaped `BriefEvent`.

## Reason

Why a brief landed in `BriefState::Failed`. Tagged enum with five
variants: `BudgetExhausted` (retry counter exceeded `RetryBudget.max`,
or the daemon raised `BudgetExhausted` from a token/wall-clock cap),
`AbortRequested` (a human or supervising agent issued an abort —
carries `actor` and `message` so the dashboard surfaces who and why),
`AcceptanceFailed` (a quality gate — coder, ac-verifier, or reviewer —
returned a non-rework failure verdict that does not warrant retry),
`PreflightSmell` (preflight-criterion-agentry's smell heuristics fired
on the brief's `success_criteria` — see below), and `DaemonError` (an
internal substrate failure with a free-text `detail` field; reserved
for the daemon's own bookkeeping mistakes, not for agent-side
problems). The discriminator is `kind` so dashboards can render a
typed badge per failure mode without parsing prose.

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

The rule applied at a phase fan-in to fold a multiset of role verdicts
into a single decision. Three variants: `AllMustPass` — every role-kind
in `expected_roles` must report `Shipped`; any other verdict triggers
`Rework` (soft fails) or `Reject` (hard fails). `FailFast` — the same
short-circuit verdicts but evaluated as evidence arrives, without
waiting for siblings; the first non-`Shipped` verdict transitions the
brief immediately. `Majority { threshold_pct }` — the `Shipped` count
must reach the threshold percentage of expected roles to `Pass`; soft
fails after the threshold is unreachable trigger `Rework`; hard fails
always `Reject`.

## GateConfig

Pairs a `GatePolicy` with the list of role-kinds the gate waits on.
`expected_roles` enumerates the role-kinds (the output of
`lifecycle::role_kind`) that must appear in the received verdict
multiset for the gate to reach a terminal `Pass` outcome. The shape is
generic — the verifier phase under `agentry-self-host-v0` carries the
three `ac-verifier-*` kinds, but `GateConfig` accepts any list.

- depends on: GatePolicy

## PhaseGates

Per-brief container for the verifying-phase and reviewing-phase
`GateConfig` values. The daemon will populate this from team topology
at brief dispatch time (landed in 396b) so each brief carries the gate
shape its topology fans out — three verifiers and two reviewers under
`agentry-self-host-v0`, different counts and policies under other
topologies.

- depends on: GateConfig

## Decide

The return value of the pure `decide` function that folds a phase's
collected verdicts against its `GateConfig`. Four variants: `Wait`
(collect more evidence), `Pass` (gate satisfied — advance), `Rework`
(soft failure — re-author the brief), `Reject` (hard failure —
terminate the brief). `Decide` is a transient return value; it is not
persisted, not serialized, and does not appear in `BriefStateRecord`.

## CiState

The CI status carried by a `BriefEvent::CiResult`. Three variants —
`Pending`, `Success`, `Failed` — matching the gitea poller's coarse
view. `Success` transitions a watching brief to `Shipped`; `Failed`
kicks off a rework loop or short-circuits to `BudgetExhausted`;
`Pending` is a no-op that keeps the brief in `Watching` so the
projector still records the poll for observability.

## ReworkTarget

Which role re-runs when the FSM enters `Reworking`. `Coder` re-spawns
the coder against the same brief with the latest blocker findings
attached; `Reviewer` re-runs the deterministic review fences against
the unchanged diff (used when only the review-side judgement was the
problem, not the code itself). The rework loop dispatched by the daemon
reads this field to pick which role to fire next.

## InvalidTransition

Returned by `handle` when an event is not legal in the current state.
Carries an owned snapshot of both the offending state and the event
that triggered the rejection so the daemon can log the pair without
re-borrowing the originals. Marked `Clone + PartialEq` so tests can
compare the rejection shape and the daemon can attach the value to a
trace event.

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
of `Authoring`. The mapping table:

| Spawned role-name shape           | Short kind     | Done-branch BriefEvent                              |
|-----------------------------------|----------------|-----------------------------------------------------|
| `coder-*`                         | `coder`        | `CoderStarted` (on spawn), `CoderDone` (on done)    |
| `ac-verifier-*`                   | `ac-verifier`  | `AcVerifierDone`                                    |
| `verifier-*`                      | `verifier`     | `AcVerifierDone`                                    |
| `reviewer-*`                      | `reviewer`     | `ReviewerDone { findings: [] }`                     |
| `shipper-agentry` (exact)         | `shipper`      | `ShipperDone`                                       |
| `ci-watcher-agentry` (exact)      | `ci-watcher`   | `CiResult { state: <verdict→CiState> }`             |
| `preflight-criterion-*`           | `preflight`    | (none — preflight emits its own typed BriefEvent)   |

CI-watcher verdict mapping: `EventVerdict::Shipped → CiState::Success`,
`EventVerdict::Failed → CiState::Failed`, all other verdicts (`Escalated`,
`ReworkNeeded`, `Rejected`) → `CiState::Pending`. The watcher does not
normally emit the latter three on its happy path; the Pending fallback
keeps the brief in `Watching` so the next poll tick can advance it.

Unrecognised role names are NOT memoized — the `Done` lookup falls
through to the catch-all (no `BriefEvent` emitted), preserving the
"unknown role is invisible" invariant rather than silently
mis-classifying a future role family.

#### FSM transition flow (not enforced by graph-specs)

```
Submitted
  → Authoring   (CoderStarted)
  → Verifying   (CoderDone Shipped)
  → Reviewing   (AcVerifierDone Shipped)
  → Shipping    (ReviewerDone Shipped)
  → Watching    (ShipperDone)
  → Shipped     (CiResult Success)
```

`Watching` is a self-loop on `CiResult Pending` (the gitea poller
is still waiting on green). `Watching → Reworking` triggers on
`CiResult Failed`, bumping the retry budget. `Authoring → Shipped`
short-circuits via `CoderDoneNoOp` when the coder's acceptance check
passes against an empty diff (no work to verify, review, or ship).

#### Gate policy and evidence accumulation (planned — #396)

The current FSM transitions on the first matching event for each phase:
the first `AcVerifierDone` advances `Verifying → Reviewing`, the first
`ReviewerDone` advances `Reviewing → Shipping`. Under the
`agentry-self-host-v0` topology — which fans out three `ac-verifier-*`
roles in the verifier phase and two `reviewer-*` roles in the reviewer
phase — this silently drops the 2nd and 3rd verifiers' reports and the
2nd reviewer's report. It looks like an observability bug (the
verdicts never reach the dashboard) but it is a correctness bug: a
brief that the first verifier waved through can have a hard `Rejected`
sitting in a sibling verifier's stdout that the FSM never sees.

#396a (this brief) lands the additive precursor: the `GatePolicy`,
`GateConfig`, `PhaseGates`, and `Decide` types plus the pure
`decide(received, gate) -> Decide` function. No behavior change yet —
`handle()` and `BriefState` are untouched, the new types are not
threaded anywhere, the existing tests in `crates/orchestrator-types/
tests/lifecycle.rs` continue to pass without edits.

#396b will migrate `BriefState::Verifying` and `BriefState::Reviewing`
to carry the received-verdict multiset (`BTreeMap<String, EventVerdict>`)
and the per-phase `GateConfig`. `handle()` will fold each new
`AcVerifierDone` / `ReviewerDone` into the multiset and call `decide`;
the FSM only transitions out of the phase when `decide` returns
`Pass`, `Rework`, or `Reject`. The lifecycle driver will fail the
brief on `InvalidTransition` rather than silently swallowing
out-of-state events — closing the silent-drop bug at both layers.

396b-1 has landed the `BriefEvent::AcVerifierDone` /
`BriefEvent::ReviewerDone` `role_name` field as the multiset-key
prerequisite — `handle()` still uses the existing serial-first-event
semantics and pattern-matches the new field with `..` so behavior is
unchanged. 396b-2 will land the `BriefState` evidence shape and the
3-arg `handle()` that consumes `role_name` to key the multiset.

396b-2 has landed the `BriefState` evidence shape (`received` /
`expected` / `policy` on `Verifying` and `Reviewing`), the third
`&PhaseGates` argument to `handle()`, and the daemon's
`build_phase_gates(team)` projection that walks `team.roles` to derive
each phase's `expected_roles` list (verifier-kind roles → verifying
gate, reviewer-kind roles → reviewing gate). Policy is currently
hardcoded to `AllMustPass` for both phases — Pattern 3 (#397) will
lift this to per-edge config in topology JSON. The transition logic in
`handle()` is still serial-first-event; 396b-3 swaps that for
evidence-based gating via `decide()`.

396b-3 lands the behavior change: `handle()` Verifying and Reviewing
arms accumulate `received` verdicts and call `decide()` to determine
Wait/Pass/Rework/Reject; `lifecycle_driver` fails the brief on
`InvalidTransition`. With this slice merged, the
`agentry-self-host-v0` ensemble (3 ac-verifiers + 2 reviewers under
`AllMustPass`) is correct: every sibling's verdict is captured, no
Failed report is silently dropped. Pattern 3 (#397) lifts the
hardcoded `AllMustPass` policy to per-edge config in the topology
JSON.
