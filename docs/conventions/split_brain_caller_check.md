# Split-brain caller check

> Status: convention. Implemented as the `CallersZero` row of
> `FENCE_MATRIX` in `agentry-role-runtime` (see
> [`specs/concepts/reviewer_fence`](../../specs/concepts/reviewer_fence.md)).
> Located under `docs/conventions/` per #294 — graph-specs only walks
> `specs/concepts/*.md` (see `specs/dialect.md`), and the convention's
> heading vocabulary (NewPubItem, CallerSet, etc.) is doc-level
> naming, not Rust pub types. The fence variant itself
> (`FenceKind::CallersZero`) lives in `reviewer_fence.md` because
> `FenceKind` IS a Rust pub type.
>
> Cross-references:
> - [`test_separation`](test_separation.md) — sister convention, also
>   relocated under #294.
> - [`specs/concepts/reviewer_fence`](../../specs/concepts/reviewer_fence.md)
>   — the deterministic fence pipeline this check participates in.

agentry's convention for *structural detection of new pub items that
lack callers in the workspace*. A new `pub fn`, `pub struct`,
`pub enum`, or other public item introduced in a brief's diff with
**zero callers in the workspace** is a split-brain candidate: the
coder either forgot to wire the new code into existing call chains
(the new helper is dangling) OR wrote a parallel implementation
alongside an existing one that still services callers (the new item
duplicates work that nobody invokes).

This connects to the architectural principle expressed in the user's
global `CLAUDE.md` `/discover` and `/prescribe` skills: every new
code addition must answer the **WIRE-THROUGH** question — does this
code integrate into existing call chains, or does it create a
parallel implementation that the existing callers don't reach? Zero
callers is the structural form of "you forgot to wire it" or "you
built a duplicate."

The check runs at reviewer-pre-pass time, AFTER the `pub-surface`
query identifies new pub items in the diff and BEFORE the LLM review
runs. It is part of the deterministic fence pipeline, not Claude's
semantic review.

## The fence variant

One row in `FENCE_MATRIX`:

```rust
(FenceKind::CallersZero, Threshold::EqualTo(0), Severity::Blocker)
```

For each new pub item produced by the diff's `pub-surface` query, the
fence runs `ra-query callers <file>:<line>:<col>` and asserts
`callers.len() > 0`. Zero callers fires a `Severity::Blocker`
`ReviewFinding` with
`origin: FindingOrigin::Mechanical { tool: "ra-query", rule: Some("callers_zero") }`.

## NewPubItem — the differential pub surface

A pub item appearing in the brief's diff but not in `origin/develop`'s
pub surface. The detection is differential: query
`ra-query pub-surface` against the post-diff file, query the same
against the pre-diff (`origin/develop`) version, take the set
difference. Each item carries `(file, line, col, name, kind)` where
`kind ∈ {Function, Struct, Enum, Trait, Const, ...}`.

The fence MUST query callers using the **position form**
(`<file>:<line>:<col>`), NEVER bare-name. Bare-name lookup is
undocumented ra-query behavior; under the fence's fail-closed posture,
name collisions could false-block valid briefs.

## CallerSet — the post-diff workspace state

The result of `ra-query callers <file>:<line>:<col>` — the
workspace's set of call sites that reference the queried position.
Empty set (`callers.len() == 0`) is the violation.

The check runs against the **post-diff state** of the workspace —
i.e. the coder's own additions count as callers if they exist (a new
pub item called from new code in the same diff is correctly wired,
not split-brain).

## Operational invariants

### Position form, never bare-name

Every callers query passes `<file>:<line>:<col>` coordinates from
`pub-surface` JSON output.

**Why:** bare-name lookup is undocumented ra-query behavior. Under
the fence's fail-closed posture, a wrong-zero result from name
collision would false-block valid briefs; a wrong-nonzero from
collision would false-pass split-brain code. Position form is the
documented contract.

### Differential pub-surface scope

The check fires only on items that are pub in the post-diff state but
were not pub in `origin/develop`. Items already pub before the diff
are out of scope (their existing callers, if any, are not the brief's
business).

**Why:** the fence-scope is "coder's diff" per the council grill
resolution; existing pub items with zero callers are pre-existing
debt for the operator's sweep stream, not the per-brief fence.

### Self-reference counts

A new pub item called from new code in the same diff is correctly
wired. The check counts callers in the workspace's post-diff state,
not the pre-diff state.

**Why:** briefs that introduce a helper AND its first caller in the
same diff are correct integration; the fence must not false-block
them.

### `callers_zero` rule string

When the fence emits a violation, the `ReviewFinding.origin` is
`FindingOrigin::Mechanical { tool: "ra-query", rule: Some("callers_zero") }`.
Consumers (dashboard, future debt-gauge if built) match on this rule
string.

**Why:** stable rule identifier across the fence matrix; consumers
don't pattern-match on prose finding messages.

### WIRE-THROUGH connection

This fence is the structural form of the user's `/discover` skill's
WIRE-THROUGH decision: every new pub item must have at least one
caller in the workspace, otherwise it is a parallel implementation
candidate or a dangling helper.

**Why:** Claude's coder bias includes "split-brain" — writing new
code in parallel with existing rather than wiring into existing
chains. The reviewer fence catches this structurally; the
`/prescribe` skill catches it during planning. Defence in depth:
prescription prevents at design time, fence prevents at review time.

### Coexistence with `dead-pub-check`

The coder's existing `dead-pub-check` (run at coder exitpoint via
#161 wave 1) and this reviewer-side fence are NOT redundant. They
use different tools (cargo build inference vs ra-query rust-analyzer
query) and run at different points (pre-exit vs pre-review). Both
run; the reviewer fence is the structural truth.

**Why:** coder-side check is a fast-fail courtesy to the coder;
reviewer-side check is the authoritative gate. The two
implementations may disagree at edges (feature-gated callers,
conditional compilation); reviewer wins. Migrating `dead-pub-check`
to ra-query position-form is filed as follow-on work, not a
fence-EPIC blocker.

## Example: a split-brain violation

```rust
// crates/foo/src/lib.rs — diff vs origin/develop adds:
pub fn parse_findings_v2(input: &str) -> Result<Findings, ParseError> {
    // new parser, but no caller in the diff or elsewhere
    ...
}
```

`ra-query pub-surface` on the post-diff file returns
`parse_findings_v2` at `(crates/foo/src/lib.rs, 42, 8)`. The
pre-diff pub surface from `origin/develop` does not contain it →
`NewPubItem`. `ra-query callers crates/foo/src/lib.rs:42:8` returns
an empty set. The fence emits a `Blocker` finding with
`rule: "callers_zero"`; the brief is rejected at pre-review.

The coder either: (a) wires the new function into an existing call
site (resolves split-brain), (b) deletes the unused function
(removes dead code), or (c) escalates if the function is genuinely
intended for an external consumer not yet present (rare — typically
indicates the brief scope was wrong).
