# Split-brain caller check

> Status: **draft**. Code landing PR: TBD (EPIC Y first brief). Council
> deliberation at `council/reviewer-fences/synthesis.md`. The fence
> wires through the reviewer's `run_fence` pipeline; spec ratifies on
> the brief that introduces the `CallersZero` fence variant in
> `FENCE_MATRIX` and the `pub-surface` → `callers` query chain in the
> deterministic-Findings conversion.

The bounded context that owns *structural detection of new pub items
that lack callers in the workspace*. A new `pub fn`, `pub struct`,
`pub enum`, or other public item introduced in a brief's diff with
**zero callers in the workspace** is a split-brain candidate: the
coder either forgot to wire the new code into existing call chains
(the new helper is dangling) OR wrote a parallel implementation
alongside an existing one that still services callers (the new item
duplicates work that nobody invokes).

This connects to the architectural principle expressed in the user's
global CLAUDE.md `/discover` and `/prescribe` skills: every new code
addition must answer the **WIRE-THROUGH** question — does this code
integrate into existing call chains, or does it create a parallel
implementation that the existing callers don't reach? Zero callers
is the structural form of "you forgot to wire it" or "you built a
duplicate."

The check runs at reviewer-pre-pass time, AFTER the `pub-surface`
query identifies new pub items in the diff and BEFORE the LLM review
runs. It is part of the deterministic fence pipeline, not Claude's
semantic review.

## SplitBrainCallerCheck

The fence variant. One row in `FENCE_MATRIX`:

```rust
(FenceKind::CallersZero, Threshold::EqualTo(0), Severity::Blocker)
```

For each `NewPubItem` produced by the diff's `pub-surface` query, the
fence runs `ra-query callers <file>:<line>:<col>` and asserts
`callers.len() > 0`. Zero callers fires a `Severity::Blocker`
`ReviewFinding` with
`origin: FindingOrigin::Mechanical { tool: "ra-query", rule: Some("callers_zero") }`.

- depends on: NewPubItem
- depends on: CallerSet
- returns: Option<ReviewFinding>

## NewPubItem

A pub item appearing in the brief's diff but not in `origin/develop`'s
pub surface. The detection is differential: query
`ra-query pub-surface` against the post-diff file, query the same
against the pre-diff (`origin/develop`) version, take the set
difference. Each item carries `(file, line, col, name, kind)` where
`kind ∈ {Function, Struct, Enum, Trait, Const, ...}`.

The fence MUST query callers using the **position form**
(`<file>:<line>:<col>`), NEVER bare-name. Bare-name lookup is
undocumented ra-query behavior (see `dead_pub_check.rs:114` for the
existing-but-unsafe pattern); under fail-closed, name collisions could
false-block valid briefs.

- depends on: PubSurface

## CallerSet

The result of `ra-query callers <file>:<line>:<col>` — the workspace's
set of call sites that reference the queried position. Empty set
(`callers.len() == 0`) is the violation.

The check runs against the **post-diff state** of the workspace —
i.e. the coder's own additions count as callers if they exist (a new
pub item called from new code in the same diff is correctly wired,
not split-brain).

## Context Mapping

This concept does NOT cross bounded contexts. It is a sub-fence within
the `reviewer_fence` context, sharing its deterministic pipeline and
fail-closed posture.

## Operational invariants (not enforced by graph-specs)

- **Position form, never bare-name.** Every callers query passes
  `<file>:<line>:<col>` coordinates from `pub-surface` JSON output.
  **WHY:** bare-name lookup (used by `dead_pub_check.rs:114` today) is
  undocumented ra-query behavior. Under the fence's fail-closed
  posture, a wrong-zero result from name collision would
  false-block valid briefs; a wrong-nonzero from collision would
  false-pass split-brain code. Position form is the documented
  contract.
- **Differential pub-surface scope.** The check fires only on items
  that are pub in the post-diff state but were not pub in
  `origin/develop`. Items already pub before the diff are out of
  scope (their existing callers, if any, are not the brief's
  business). **WHY:** the fence-scope is "coder's diff" per the grill
  resolution; existing pub items with zero callers are pre-existing
  debt for the operator's sweep stream, not the per-brief fence.
- **Self-reference counts.** A new pub item called from new code in
  the same diff is correctly wired. The check counts callers in the
  workspace's post-diff state, not the pre-diff state. **WHY:**
  briefs that introduce a helper AND its first caller in the same
  diff are correct integration; the fence must not false-block them.
- **`callers_zero` rule string.** When the fence emits a violation,
  the `ReviewFinding.origin` is
  `FindingOrigin::Mechanical { tool: "ra-query", rule: Some("callers_zero") }`.
  Consumers (dashboard, future debt-gauge if built) match on this
  rule string. **WHY:** stable rule identifier across the fence
  matrix; consumers don't pattern-match on prose finding messages.
- **WIRE-THROUGH connection.** This fence is the structural form of
  the user's `/discover` skill's WIRE-THROUGH decision: every new pub
  item must have at least one caller in the workspace, otherwise it
  is a parallel implementation candidate or a dangling helper.
  **WHY:** Claude's coder bias, documented in
  `feedback_claude_coder_biases.md`, includes "split-brain" — writing
  new code in parallel with existing rather than wiring into existing
  chains. The reviewer fence catches this structurally; the
  `/prescribe` skill catches it during planning. Defence in depth:
  prescription prevents at design time, fence prevents at review
  time.
- **Coexistence with `dead-pub-check`.** The coder's existing
  `dead-pub-check` (run at coder exitpoint via #161 wave 1) and this
  reviewer-side fence are NOT redundant. They use different tools
  (cargo build inference vs ra-query rust-analyzer query) and run at
  different points (pre-exit vs pre-review). Both run; the reviewer
  fence is the structural truth. **WHY:** coder-side check is a
  fast-fail courtesy to the coder; reviewer-side check is the
  authoritative gate. The two implementations may disagree at edges
  (feature-gated callers, conditional compilation); reviewer wins.
  Migrating `dead-pub-check` to ra-query position-form is filed as
  follow-on work, not a fence-EPIC blocker.
