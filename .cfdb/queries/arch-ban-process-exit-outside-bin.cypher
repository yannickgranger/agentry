// arch-ban-process-exit-outside-bin.cypher — ban std::process::exit calls
// outside crate `src/bin/` entry points.
//
// Rationale: process::exit skips Rust destructors (Drop, async shutdown,
// Mutex poison cleanup). Library/daemon/runtime code must propagate
// errors via Result so the caller decides whether to exit, retry, or
// convert to a structured failure event. Only the binary entry point
// (the file containing fn main) is allowed to terminate the process via
// process::exit.
//
// The cfdb extractor emits a CallSite with callee_path =
// 'std::process::exit' for fully-qualified calls. We filter on:
// - cs.callee_path equals exactly the qualified path (textual match
//   against the author-written form; HIR-resolved variants surface as
//   the same canonical path).
// - caller.is_test = false — exclude #[cfg(test)] callers.
// - cs.is_test = false — exclude #[cfg(test)] call expressions.
// - NOT cs.file =~ '.*/bin/.*\\.rs' — exclude src/bin/ entry-point files.
//
// Verified zero violations on develop tip.
//
// Expected: 0 violations on a clean tree. Any row is a violation.

MATCH (caller:Item)-[:INVOKES_AT]->(cs:CallSite)
WHERE cs.callee_path = 'std::process::exit'
  AND caller.is_test = false
  AND cs.is_test = false
  AND NOT cs.file =~ '.*/bin/.*\\.rs'
RETURN caller.qname, caller.crate, cs.file, cs.line
