// arch-ban-inline-cfg-test-in-src.cypher — ban inline `#[cfg(test)]`
// items in `crates/*/src/`. Tests live in `tests/` directories
// alongside `src/`, not interleaved with production sources.
//
// Rationale: keeping test code out of `src/` keeps the production
// compile graph free of test-only items, makes `cargo test` and
// `cargo build` artifact sets disjoint, and prevents `#[cfg(test)]`
// helpers from accidentally leaking into doc/rustdoc surfaces. The
// agentry layout convention is: every crate's tests live in its
// peer `tests/` directory; `src/` is production-only.
//
// The cfdb extractor sets `Item.is_test = true` when the item is
// under a `#[cfg(test)]` module or directly annotated `#[test]`
// (council-cfdb-wiring §B.1.1). The extractor walks `src/` only —
// `tests/` integration files are not extracted into `:Item` nodes
// — so in practice every `is_test = true` item already lives under
// some `src/` path. The `i.file` regex filter is kept anyway as a
// belt-and-suspenders guard so the rule remains correct if the
// extractor's coverage widens to include `tests/` in a future
// schema bump.
//
// Filters:
// - `i.is_test = true` — the item is `#[cfg(test)]`-scoped or
//   `#[test]`-annotated.
// - `i.file =~ '.*/crates/[^/]+/src/.*'` — the defining file lives
//   under some crate's `src/` directory. The leading `.*/` tolerates
//   both the workspace-relative form (`crates/foo/src/bar.rs`) and
//   the absolute form the extractor emits in CI containers
//   (`/workspace/crates/foo/src/bar.rs`); cfdb's path normalization
//   differs across environments and the unwrap rule (#first ban
//   rule) flagged the same fragility.
//
// Exemptions: none. Every `#[cfg(test)]` item in `src/` is a
// violation; move it to a peer `tests/` file.
//
// Usage:
//   cfdb violations --db <dir> --keyspace agentry \
//       --rule .cfdb/queries/arch-ban-inline-cfg-test-in-src.cypher
//
// Expected: 0 violations on a clean tree. Any row is a violation.

MATCH (i:Item)
WHERE i.is_test = true
  AND i.file =~ '.*/crates/[^/]+/src/.*'
RETURN i.qname, i.crate, i.file, i.line
