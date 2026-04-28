# Validation

The bounded context that owns *deterministic gates run against a candidate
diff before the daemon accepts a brief*. Distinct from `review` —
`ReviewFinding` is the structured record of *any* gate (mechanical or
model); `Validation` is the pipeline of mechanical gates wired per
`BriefKind` and run by the ship tool.

Brief 2 of EPIC #152 introduces the type-level scaffolding only. Stub
implementations return a passing report; brief 3 ports the existing
reviewer-mechanical logic into real `Validator` impls; brief 4 wires the
ship binary to dispatch via `validators::registry_for(brief.kind)`.

## Validator

The trait every gate implements. Object-safe, async, fallible. Each impl
declares a stable `name()` (used in dashboards and reports) and a `run()`
that consumes a `BriefCtx` and returns a `ValidatorReport`. The trait is
intentionally agnostic of run mechanism — implementations may shell out
to cargo, hit a container, walk the diff in-process, or call a remote
service. The pipeline holds `&'static dyn Validator`s — every concrete
implementation is a unit-struct singleton, addressable by static
reference.

## BriefCtx

The per-brief execution context handed to every validator: the workspace
path to inspect, the brief id (for log + report attribution), and the
list of files changed by the coder vs the base branch. The ship tool
populates `changed_files` from `git diff --name-only`; stubs and tests
leave it empty.

## ValidatorReport

The record a validator produces. Names the validator that produced it,
records pass/fail, and carries zero or more `Finding`s. Pass means no
findings worth surfacing; fail means at least one blocker — the daemon
treats a failing validator pipeline as `VerdictKind::ReworkNeeded` and
routes findings back to the upstream coder via the team's
`message_graph`. Constructors `pass(name)` and `fail(name, findings)`
keep the happy path concise.
