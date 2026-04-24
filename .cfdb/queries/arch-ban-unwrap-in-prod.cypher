// arch-ban-unwrap-in-prod.cypher — first methodology ban rule for agentry.
//
// Surfaces every call site in prod code whose callee path is exactly
// `unwrap` — i.e. `.unwrap()` invocations that panic on `None`/`Err`.
// Production code must propagate errors via `?` or attach context via
// `.expect("…")`; `.unwrap()` is for tests and quick prototypes only.
//
// The cfdb extractor emits a `CallSite` node per call expression with a
// `callee_path` property carrying the textual callee the author wrote.
// For a method call like `foo.unwrap()`, `callee_path = 'unwrap'` (the
// method identifier, without the receiver). Exact equality is used rather
// than a regex so `.unwrap_or(..)`, `.unwrap_err(..)`, and
// `Option::unwrap(..)` (path call, rare) do not match — only the bare
// panicking method.
//
// Filters:
// - `caller.is_test = false` — the enclosing fn/impl-method must not be
//   under `#[cfg(test)]`.
// - `cs.is_test = false` — the call expression itself must not be in a
//   test-gated block.
//
// Tests legitimately need `.unwrap()` on test fixtures; this exemption is
// load-bearing. The filter is not "exclude tests/ directory" — it is
// "exclude cfg(test) scopes", which is more precise (a non-test fn that
// happens to live in a `tests/` file still fails this rule if the
// extractor tags it prod).
//
// Usage:
//   cfdb violations --db <dir> --keyspace agentry \
//       --rule .cfdb/queries/arch-ban-unwrap-in-prod.cypher
//
// Expected: 0 violations on a clean tree. Any row is a violation.

MATCH (caller:Item)-[:INVOKES_AT]->(cs:CallSite)
WHERE cs.callee_path = 'unwrap'
  AND caller.is_test = false
  AND cs.is_test = false
RETURN caller.qname, caller.crate, cs.file, cs.callee_path
