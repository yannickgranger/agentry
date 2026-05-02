# Test separation

> Status: convention. Structurally enforced by the cfdb ban rule
> `.cfdb/queries/arch-ban-inline-cfg-test-in-src.cypher` (shipped via
> X.0, commit `d2fb577`). Located under `docs/conventions/` per #294 —
> graph-specs only walks `specs/concepts/*.md` (see `specs/dialect.md`),
> and the convention's heading vocabulary (TestSeparation,
> TestsDirectory, PublicSurfaceTesting) is doc-level naming, not
> Rust pub types.
>
> Cross-references:
> - [`split_brain_caller_check`](split_brain_caller_check.md) — sister
>   convention, also relocated under #294.
> - [`specs/concepts/reviewer_fence`](../../specs/concepts/reviewer_fence.md)
>   — the `FenceKind` / `Threshold` / `FENCE_MATRIX` types whose
>   reviewer pass depends on the `tests/`-only convention for its path
>   filter (`**/tests/*.rs`) to be exact.

agentry's workspace-wide convention for *the physical and conceptual
layout of test code in agentry crates*. agentry adopts a **stricter**
convention than qbot-core: where qbot-core uses BOTH `tests/`
directories and inline `#[cfg(test)]` blocks (609 src files with
inline `cfg(test)` across qbot-core), agentry permits only the
`tests/` form.

The convention has two prongs:

1. **Physical.** All tests live in `tests/<file>.rs` at the crate
   root. No `#[cfg(test)] mod tests {}` blocks in `src/*.rs`. Existing
   inline tests migrated into `tests/` directories per the EPIC #256
   migration stream.
2. **Conceptual.** Tests cover only the **public API** of the crate.
   Private helpers are covered transitively through the public surface
   they support; if a private helper is complex enough to need direct
   testing, that is a design signal — refactor to make the behavior
   testable through the public API, OR extract the helper into its own
   `pub` module/function with a clear external contract.

The convention's rationale connects to ISP at the test boundary: tests
test the unit's *boundary*, not its internals. Forcing tests to live
in `tests/` (which compiles as a separate integration-test crate,
seeing only the lib's `pub` surface) makes private-helper testing
mechanically impossible — the design pressure forces tests to align
with the unit's actual public contract.

## TestsDirectory — physical location requirement

Each crate's tests live under `crates/<name>/tests/*.rs`. Files in
`src/*.rs` MUST NOT contain `#[cfg(test)]` blocks.

Enforcement: the cfdb ban rule
`.cfdb/queries/arch-ban-inline-cfg-test-in-src.cypher` (X.0) refuses
new inline `#[cfg(test)]` in `src/*.rs`. The rule landed with zero
existing violations, per the project's "no baseline, no ratchet"
policy.

## PublicSurfaceTesting — conceptual rule

Test bodies invoke only items reachable through the crate's `pub`
surface. `pub(crate)` widening solely to expose private helpers to
tests is forbidden — promotion-without-movement is rejected.

If a helper genuinely belongs on the public surface (because external
consumers need it), promote AND move into a stable module location in
the same step. If a helper is a private implementation detail, leave
it private and trust that the public-surface tests cover its behavior
transitively.

## Operational invariants

### No `#[cfg(test)]` in `src/`

Every `src/*.rs` file is free of `#[cfg(test)]` blocks. The migration
moved existing blocks to `tests/`; the cfdb ban rule prevents new
ones.

**Why:** the path-filter for the reviewer fence (`**/tests/*.rs`)
becomes exact and stable. Inline blocks would force `syn`-based
block-range parsing in the reviewer, which the rust-systems lens
hard-rejects. The convention is its own enforcement mechanism; the
fence depends on it.

### Tests test the public API only

Test files in `crates/<name>/tests/` import via `use <crate_name>::*`
and exercise the lib's `pub` surface. They DO NOT use `pub(crate)`
items (which are unreachable from `tests/` for a binary target
anyway, and from a lib target are an architectural smell when
reached for tests).

**Why:** ISP at the test boundary. If private helpers need direct
testing, the design needs reshaping — extract the helper as a pub
item with a real contract, or fold it into a wider pub surface that
already tests its behavior. Promote-for-test-only is an anti-pattern.

### `pub(crate)` widening to satisfy tests is forbidden

Migrations MUST NOT take inline tests of private functions and
promote those functions to `pub(crate)` mechanically to make them
reachable.

**Why:** promotion-without-movement is a widened internal contract
with no corresponding value. The function's privacy was the symptom;
the test design is the disease. Either:

- (a) the function belongs on the lib's public surface and gets
  moved + promoted in one step,
- (b) the function stays private and is covered transitively through
  the public surface tests, or
- (c) the test gets deleted as redundant with public-surface coverage.

### `[[bin]]` crate test mechanics

For binary targets, `tests/` files cannot `use` ANY items from the
binary's modules (`pub(crate)` does not span the boundary). Two
legitimate test shapes:

- **Lib-level integration tests.** Move fence-pure / domain-pure
  logic from `src/bin/*.rs` into `src/lib.rs` as `pub fn`, then
  `tests/` files import from the lib normally.
- **Subprocess integration tests.** `tests/<binary>_test.rs` spawns
  the binary via `std::process::Command`, pipes crafted JSON to
  stdin, asserts on stdout.

**Why:** binary `tests/` visibility is a Rust compilation rule, not a
convention question. The migration brief for `agentry-role-runtime`
used both shapes — domain-pure functions (`parse_findings`,
`run_fence`) moved to lib for direct integration testing; binary
end-to-end behavior (stdin/stdout contract, fail-closed under missing
ra-query) got subprocess tests.

### Lib extraction is the migration first step for binary crates

For crates with binary targets containing testable logic (currently
`agentry-role-runtime` is the canonical case), the migration brief
sequenced as: (1) extract domain-pure logic into `src/lib.rs` as
`pub`, (2) write `tests/` files against the lib's pub surface, (3)
delete inline test blocks. Steps 2 and 3 cannot precede step 1.

**Why:** binary `tests/` files compile as separate crates; without
the lib extraction first, the migration would fail compilation. This
was flagged as `rust-systems N3` and `clean-arch N5` in the council's
r1, convergent in r2 across all three lenses.

### No `syn` runtime dependency in the reviewer

Path-filter (`**/tests/*.rs`) is sufficient under tests/-only. If a
future proposal suggests adding `syn` for inline-block detection, the
answer is to enforce the convention, not parse around it.

**Why:** `syn` is a heavyweight proc-macro library; adding it to a
runtime container binary duplicates rust-analyzer's parse work for no
gain. Hard non-negotiable from rust-systems N1.

## Example: lib + tests/ shape

```rust
// crates/foo/src/lib.rs
pub fn parse_findings(input: &str) -> Result<Findings, ParseError> { ... }

fn normalize_whitespace(s: &str) -> String { ... } // private helper
```

```rust
// crates/foo/tests/parse_findings.rs
use foo::parse_findings;

#[test]
fn rejects_malformed_json() {
    let err = parse_findings("{bad").unwrap_err();
    assert!(matches!(err, ParseError::Json(_)));
}
```

`normalize_whitespace` is exercised transitively through
`parse_findings` callers. No `pub(crate)` widening; no inline test
module in `src/lib.rs`.
