# Brief retry budget

> Status: **ratified**. Code landing PR: L.1 (EPIC #246). The
> `RetryBudget` type and the `DEFAULT_ATTEMPT_CAP` /
> `MAXIMUM_ATTEMPT_CAP` constants live in
> `crates/orchestrator-types/src/lifecycle.rs`. The L.1 FSM is the
> first consumer; future topologies validate `max_retries` against the
> hard ceiling at register time.

The bounded context that owns *how many times a brief may bounce
between rework and the upstream worker before the orchestrator gives
up*. Every non-terminal `BriefState` variant carries a `RetryBudget`;
each rework-loop transition increments `attempt`; when the next value
would breach `max`, the FSM short-circuits to
`BriefState::Failed { reason: Reason::BudgetExhausted }` instead of
emitting the proposed next state.

The retry budget is *separate from* the wall-clock and token budgets
on `Brief.budget` (see `specs/concepts/briefing.md`). Token / wall
budgets are external observations the daemon raises as
`BriefEvent::BudgetExhausted`; the retry budget is internal to the FSM
and consumed by the rework-loop transitions. Both end at the same
terminal — `Failed { BudgetExhausted }` — so dashboards do not need
to distinguish "ran out of attempts" from "ran out of money", and
operators reading the trace stream can see which limiter actually
fired by looking at the event log directly.

`max` is typically copied from the `TeamTopology.max_retries` setting
when the brief is dispatched. Topologies that omit `max_retries`
inherit `DEFAULT_ATTEMPT_CAP`; topologies declaring a value above
`MAXIMUM_ATTEMPT_CAP` are rejected at dispatch time as an acceptance
failure (no team should be able to spin forever even if it asked to).

## RetryBudget

The counter itself: a `{attempt: u32, max: u32}` pair. `attempt` is
1-based — the first authoring run is `attempt=1`. The `handle`
transition function never decreases `attempt`; transitions that do
not represent a fresh rework attempt (e.g. `Reworking + CoderStarted`
returning to `Authoring`) preserve the existing budget unchanged.
Transitions that DO represent a fresh rework attempt
(`Verifying + AcVerifierDone(failed/rework)`,
`Reviewing + ReviewerDone(rework_needed)`,
`Watching + CiResult(failed)`) bump `attempt` by one — and short-
circuit to `Failed { BudgetExhausted }` when the bumped value would
exceed `max`. The type is `Copy` so the FSM can move the budget
around match arms without explicit clones.
