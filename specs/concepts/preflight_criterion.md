# Preflight criterion

> Status: **ratified**. Code landing PRs: brief 84a (role + runner), brief
> 84b-1 (verdict translation + `Reason::PreflightSmell`), brief 84b-2 (this
> doc + planner/verify topology wiring + submit-time shape-check). Closes
> EPIC #84.

The bounded context that owns *gating a meta-brief on the well-formedness
of its `success_criteria`*. Preflight runs once per meta-brief, before the
planner or verifier roles spawn, and either passes the criterion through
unchanged or fails the brief with `Reason::PreflightSmell` so the operator
sees that the bar itself is malformed before any expensive work happens.
Submit-time shape validation in the orchestrator CLI catches structurally
invalid criteria even earlier — before the brief is XADD'd to
`agentry:briefs`.

The intent is operator-feedback locality: a smell that fires inside a
running team takes minutes (container spawn, role bootstrap) to surface
and costs at minimum one container slot; a smell caught at submit time is
synchronous and free. Submit-time covers the shape; runtime preflight
covers the content.

The `preflight-criterion-agentry` (v1) role is read-only and unauthed.
Its runner
(`crates/agentry-role-runtime/src/bin/preflight_criterion_runner.rs`)
takes the startup bundle on stdin, reads
`brief.payload.success_criteria` and `brief.payload.target_repo`, splits
the criterion on the FIRST occurrence of ` : ` (space-colon-space) into
`(cmd, expected)` with `expected` trimmed, `cd /workspace`, runs
`bash -c "$cmd"` with stdout/stderr captured, and reports `baseline`
(trimmed stdout), `expected`, and `match` as a `baseline_match` event.
It then applies the heuristic smell-tests in order, first-smell-wins,
and emits a terminal `done` event — `shipped` on success, `failed` on
smell or shape error, with `cause: "preflight_smell"` for smells. The
runner's `DoneGuard` enforces that exactly one `done` record is emitted.
The daemon's trace translator folds a `done failed` with
`cause: "preflight_smell"` into `BriefEvent::PreflightSmellDetected`,
which the FSM resolves to
`BriefState::Failed { reason: Reason::PreflightSmell }` — the typed
badge dashboards render against.

The topologies that consume `success_criteria` are
`agentry-planner-v0` (preflight → archaeologist → planner) and
`agentry-verify-v0` (preflight → verifier), both wired in brief 84b-2.
Each places `preflight-criterion-agentry` first in the role list and
adds a single edge from preflight to the previous head role. Terminal
roles are unchanged: planner-v0 terminates on `planner-claude-agentry`,
verify-v0 terminates on `verifier-claude-agentry`. Preflight is
non-terminal — its `done failed` short-circuits the brief via the FSM,
not via topology routing. When a new topology starts consuming
`success_criteria`, add its name to the
`TOPOLOGIES_WITH_SUCCESS_CRITERIA` constant in
`crates/orchestrator-runtime/src/submit_shape_check.rs` so submit-time
validation tracks the runtime topology set.

The runner applies smells in declaration order; the first blocking
smell that fires emits a `Warn` finding and a `done failed` with
`cause: "preflight_smell"` — execution stops there. Smell 1
(count-zero against a high baseline) fires when `expected == "0"`,
`baseline` is numeric and greater than 100, and `cmd` contains the
literal `wc -l` — it catches "fail when this never-zero count drops to
zero" criteria that would silently report success once the file
disappeared, the path was renamed, or a glob matched nothing
(blocking). Smell 2 (literal `grep -v 'mod tests'`) fires when `cmd`
contains the substring `grep -v 'mod tests'` — it catches the
test-scope-exclusion pattern that does not actually exclude
`#[cfg(test)]` blocks, since Rust's test scope is not always introduced
by a `mod tests` line (blocking). Smell 3 (`wc -l` without
`#[cfg(test)]` filter) fires when `cmd` contains `wc -l` and does NOT
mention `#[cfg(test)]` — it surfaces the same test-scope-exclusion gap
as smell-2 but as a Warn-only signal, since many legitimate `wc -l`
criteria do not need the filter (non-blocking). These heuristics ARE
the contract: per the brief-84b `/grill-me` transcript (Q4), there is
no operator-override mechanism — refining the heuristics is a
code-level PR against the runner, not a runtime flag.

When a smell fires the operator's response is to rewrite the criterion
to be more specific and resubmit. In order of preference: replace the
line-count proxy with a structural query (e.g. a `cfdb query` that
matches the actual symbols you care about, or a `ra-query` invocation
that respects Rust scopes); tighten the path/glob so that "zero" is
informative (a count over a single file's `pub fn` set is more useful
than a count over a whole crate); or switch the criterion's polarity —
if "drop this count to zero" is smelly, "this exact symbol is gone"
via `! grep -q` is often what you actually meant. If none of those fit,
the smell rule itself is the bug — open a PR against the runner with
the new heuristic and the rationale. The heuristics are ratchets, not
allowlists: each rule lands with zero existing violations and a
justification for what it catches.

## ShapeError

The `submit_shape_check::ShapeError` enum is the type the orchestrator's
`submit` subcommand returns when `brief.payload.success_criteria` fails
the structural gate. Three variants, one per failure mode, each carrying
a stable, operator-facing `message()` printed to stderr before a
non-zero exit: `MissingOrEmpty` covers `success_criteria` missing,
empty, or whitespace-only on a brief targeting a topology in
`submit_shape_check::TOPOLOGIES_WITH_SUCCESS_CRITERIA`;
`MissingSeparator` covers a criterion that does not contain the literal
` : ` (space-colon-space) separator the runner splits on; `EmptyExpected`
covers the right-hand side of ` : ` (after trimming) being empty, so
there is nothing for the runner to compare against.

The check runs BEFORE the brief reaches Redis; submission is atomic
with shape validation. Briefs targeting topologies outside
`TOPOLOGIES_WITH_SUCCESS_CRITERIA` (e.g. `agentry-self-host-v0`, which
carries `acceptance` instead) bypass the check entirely — they would
never be read by preflight, so a missing or malformed criterion is not
a defect for them.

The check is intentionally narrow: it covers shape only. Heuristics for
the criterion's content (what the cmd does, what value `expected`
holds) live in the runner's smell-tests above — they cannot run at
submit time without spawning a container, which is the cost the gate
exists to avoid.
