// arch-ban-rundata-phase-names.cypher — ban `RunData` variants named
// after methodology phases.
//
// Rationale: `RunData` carries per-node-class data on a `Walking` brief
// (e.g. `Coder { agent_id }`, `PrTracking { pr_number, head_sha }`,
// `OperatorDecision { disagreements }`). Variant names must describe
// the data shape, NOT the methodology phase. Naming a variant after a
// methodology phase (`Verifying`, `Reviewing`, `Shipping`, etc.) is
// methodology-in-Rust at one level of indirection — exactly the
// re-introduction risk the synthesis flagged. Per business-domain R2
// catch on C5: "if pre-built [WalkConfig], the node-class field must be
// NodeClass(String) newtype, NOT a Rust NodeKind enum — would re-
// introduce methodology in Rust at one level of indirection." Same
// principle applies to `RunData`.
//
// Cited doctrine: `feedback_no_tech_debt_for_speed` ("every tech-debt
// fix must add a code-level fence ... against re-introduction"); council
// synthesis "C1 — RunData shape", "Three cfdb ban rules", "Out-of-scope
// notes: NodeKind typed enum vs NodeClass(String)".
//
// Deny-list: the seven methodology phase names the synthesis enumerated
// (`Verifying`, `Reviewing`, `Reworking`, `Shipping`, `Watching`,
// `Authoring`, `AwaitingCaptainDecision`). The list is case-sensitive
// because Rust variant names are conventionally PascalCase.
//
// Mechanism: same as `arch-ban-briefstate-variants` — match every
// `:CallSite` construction of the form `RunData::<DenyListed>`. cfdb's
// extractor does not model enum variants as `:Item` nodes; variants
// surface at construction sites. Adding a variant requires using it
// somewhere, which surfaces as a `CallSite` callee_path. `cargo
// clippy -- -D dead_code` catches variants defined but never used.
//
// Verified zero violations on develop tip 2026-05-14 (post-#532, which
// introduced `RunData` with the synthesis-prescribed data-shape names:
// `None`, `Coder`, `PrTracking`, `OperatorDecision`, `Extension`).
//
// Usage:
//   cfdb violations --db <dir> --keyspace agentry \
//       --rule .cfdb/queries/arch-ban-rundata-phase-names.cypher
//
// Expected: 0 violations on a clean tree. Any row is a violation.

MATCH (cs:CallSite)
WHERE cs.callee_path =~ 'RunData::(Verifying|Reviewing|Reworking|Shipping|Watching|Authoring|AwaitingCaptainDecision)'
RETURN cs.callee_path, cs.file, cs.line
