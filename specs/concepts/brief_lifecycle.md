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

## Reason

Why a brief landed in `BriefState::Failed`. Tagged enum with four
variants: `BudgetExhausted` (retry counter exceeded `RetryBudget.max`,
or the daemon raised `BudgetExhausted` from a token/wall-clock cap),
`AbortRequested` (a human or supervising agent issued an abort —
carries `actor` and `message` so the dashboard surfaces who and why),
`AcceptanceFailed` (a quality gate — coder, ac-verifier, or reviewer —
returned a non-rework failure verdict that does not warrant retry),
and `DaemonError` (an internal substrate failure with a free-text
`detail` field; reserved for the daemon's own bookkeeping mistakes,
not for agent-side problems). The discriminator is `kind` so dashboards
can render a typed badge per failure mode without parsing prose.

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
