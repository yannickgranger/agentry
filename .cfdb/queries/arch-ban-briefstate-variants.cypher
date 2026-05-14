// arch-ban-briefstate-variants.cypher — ban `BriefState` variants outside
// the synthesis-ratified four: `Submitted`, `Walking`, `Shipped`, `Failed`.
//
// Rationale: PR #532 (495 beta-b) collapsed the FSM's `BriefState` enum
// from 11 methodology-named variants (`Authoring`, `Verifying`, `Reviewing`,
// `Reworking`, `Shipping`, `Watching`, `AwaitingCaptainDecision`, etc.) to
// the four data-shape variants the council synthesis prescribed (one entry
// state, one mid-flight state, two terminals). Methodology lives in
// topology JSON; the FSM is a generic topology walker. Reintroducing any
// methodology-named variant — even temporarily — re-creates the runtime/
// JSON split-brain that the v2 finale dissolved.
//
// Cited doctrine: `feedback_no_tech_debt_for_speed` ("every tech-debt fix
// must add a code-level fence — cfdb rule, lint, validator, type —
// against re-introduction"). The fix shipped in #532; this is its fence.
//
// Mechanism: cfdb's extractor models enum variants as `:CallSite` nodes
// at every construction site (the enum itself is one `:Item`; its
// variants are not separate `:Item` nodes in the current schema —
// confirmed against `cfdb extract` on develop tip 2026-05-14). The rule
// matches every construction-site `cs.callee_path` of the form
// `BriefState::<Variant>` and excludes the four allowed names. Any row
// returned is a usage of a forbidden variant, which (by Rust's usage
// rules) implies the variant has been defined.
//
// Limitations:
// - Variants defined but never constructed (dead code) escape this rule;
//   `cargo clippy -- -D dead_code` catches those.
// - The rule keys off the literal callee_path string the extractor
//   records. Calls written via type aliases (e.g.
//   `type Foo = BriefState; Foo::Walking { .. }`) would surface as
//   `Foo::Walking`, not `BriefState::Walking`. No type aliases on
//   `BriefState` exist in the workspace today (verified by cfdb query
//   `MATCH (i:Item) WHERE i.kind = "type_alias" AND i.qname =~ ".*BriefState.*"
//   RETURN i.qname` returns empty); add a stricter rule if one is ever
//   introduced.
//
// Verified zero violations on develop tip 2026-05-14 (post-#532).
//
// Usage:
//   cfdb violations --db <dir> --keyspace agentry \
//       --rule .cfdb/queries/arch-ban-briefstate-variants.cypher
//
// Expected: 0 violations on a clean tree. Any row is a violation.

MATCH (cs:CallSite)
WHERE cs.callee_path =~ 'BriefState::[A-Za-z_][A-Za-z0-9_]*'
  AND NOT cs.callee_path = 'BriefState::Submitted'
  AND NOT cs.callee_path = 'BriefState::Walking'
  AND NOT cs.callee_path = 'BriefState::Shipped'
  AND NOT cs.callee_path = 'BriefState::Failed'
RETURN cs.callee_path, cs.file, cs.line
