# Reviewer fence

> Status: **draft**. Type-table landing PR: brief Y.2. The
> `agentry-role-runtime` lib now carries `FenceKind`, `UnwrapSeverity`,
> `Threshold`, and the `FENCE_MATRIX` const that drives deterministic
> reviewer fences. Y.3 wires `run_fence` to consume the matrix; Y.6
> integrates the fence pass into the reviewer container and graduates
> this spec to ratified.

The bounded context that owns *deterministic gating fences* run by the
reviewer before any model-driven review. Each fence consults `ra-query`
(rust-analyzer-on-disk), compares the result against a `Threshold`,
and emits a Blocker `FindingOrigin::Mechanical { rule }` finding when
the threshold is breached. The matrix-as-data shape keeps adding a
sixth fence to a one-row policy edit, no logic change.

## FenceKind

The fence variant. Each kind names one ra-query subcommand call and
one `rule` string for the emitted mechanical finding:

- `ClonesInLoop` — `ra-query clones`, the `clones_in_loop` field
  exceeds the threshold.
- `CloneProd` — `ra-query clones`, `clone_calls - arc_rc` outside
  loops exceeds the threshold (productive clones, not Arc/Rc shares).
- `Complexity` — `ra-query complexity`, per-function cognitive
  complexity exceeds the threshold.
- `Unwraps` — `ra-query unwraps`, severity at threshold or above.
- `CallersZero` — `ra-query callers <file:line:col>` reports zero
  callers on a newly added pub item.

## UnwrapSeverity

The severity ladder for `ra-query unwraps` output. Ordered
`Low < Medium < High < Critical`; threshold comparisons use `>=`.
Independent of `review::Severity` because `ra-query unwraps`
classifies on its own axis (call-site context, panic surface) before
the fence translates a breach into a Blocker review finding.

## Threshold

The shape of a fence comparison. Three forms cover all current
fences:

- `GreaterThan(u32)` — numeric count strictly above N (clones,
  complexity).
- `SeverityAtLeast(UnwrapSeverity)` — unwrap severity at or above
  the named rung.
- `EqualTo(u32)` — numeric count equal to N, used by `CallersZero`
  to gate dead-on-arrival pub items.
