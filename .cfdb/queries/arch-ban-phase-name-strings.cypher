// arch-ban-phase-name-strings.cypher — ban methodology-phase string
// literals in orchestrator-runtime / orchestrator-types production
// source. Methodology lives in topology JSON; the FSM is a generic
// topology walker. A bare "verifying" / "reviewing" / etc. string in
// Rust source is methodology-in-Rust at one level of indirection —
// the same drift risk the BriefState-variants and RunData-variants
// rules fence at the type level.
//
// Cited doctrine: council synthesis `council/v2-finale-fsm-collapse/
// synthesis.md` line 262-263 ("No string literal in
// crates/orchestrator-runtime or crates/orchestrator-types may match
// ^(verifying|reviewing|shipping|watching|reworking|
// awaiting_captain_decision)$ (case-insensitive), except inside the
// topology JSON parser at crates/orchestrator-types/src/team.rs").
// Feedback `feedback_no_tech_debt_for_speed` ("every tech-debt fix
// must add a code-level fence").
//
// Mechanism: cfdb's extractor (RFC-041) emits :Literal nodes per
// string literal in Rust source, carrying `value`, `crate`, `file`,
// `line`, `is_test`. Attribute-embedded strings (`#[doc=...]`) are
// out of v0 scope per RFC-041 §6; comments are not in the syn AST.
// So this rule sees only real string-literal expressions in code.
//
// Filters:
// - l.value =~ '(?i)^(verifying|reviewing|shipping|watching|reworking|awaiting_captain_decision)$'
//   — case-insensitive, exact match (anchored both ends).
// - l.crate IN ['orchestrator-runtime', 'orchestrator-types']
//   — only the two crates the synthesis names. The runtime walks
//   the FSM; the types crate defines the lifecycle vocabulary.
// - l.is_test = false — test code may name phases freely (test
//   fixture brief ids, doc tests, etc.).
// - NOT l.file =~ '.*/crates/orchestrator-types/src/team\.rs'
//   — exemption: TeamTopology / NodeClass / NodeId definitions live
//   here; if the topology JSON parser ever needs to recognize a
//   methodology-phase string (e.g. for backward-compat shim), it
//   would land here, not elsewhere. Today this file contains zero
//   phase-name literals (serde derives the parser).
//
// Verified zero violations on develop tip 2026-05-15: grep across
// orchestrator-runtime/src and orchestrator-types/src returns no
// matches for the regex.
//
// Usage:
//   cfdb violations --db <dir> --keyspace agentry \
//       --rule .cfdb/queries/arch-ban-phase-name-strings.cypher
//
// Expected: 0 violations on a clean tree. Any row is a violation.

MATCH (l:Literal)
WHERE l.value =~ '(?i)^(verifying|reviewing|shipping|watching|reworking|awaiting_captain_decision)$'
  AND l.crate IN ['orchestrator-runtime', 'orchestrator-types']
  AND l.is_test = false
  AND NOT l.file =~ '.*/crates/orchestrator-types/src/team\\.rs'
RETURN l.value, l.crate, l.file, l.line
