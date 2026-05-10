// arch-ban-parse-target-repo-free-fn.cypher — bans direct calls to the
// free function `parse_target_repo` (formerly in `redis_io.rs:457`)
// after its deletion. Routing-key parsing must go through
// `TargetRepo::from_str`, the canonical parser on the typed value.
//
// Rationale: until brief 1, three independent parse functions for the
// routing-key concept existed across two crates: `parse_target_repo`
// (`redis_io.rs:457`, returns `(&str, &str)` after non-empty check),
// `sanitize_target_repo_slug` (`anchor_resolver.rs:50`, no validation),
// and `split_target_repo` (`agentry-role-runtime::ci_watcher_runner`
// + `shipper_runner`, no validation, returns empty strings on missing
// `/`). Brief 1 collapses all three into `TargetRepo::from_str` with
// uniform charset+length validation. The free `parse_target_repo` is
// deleted in brief 1; this rule prevents reintroduction.
//
// The cfdb extractor surfaces a `:CallSite` per call expression with
// `callee_path` carrying the textual callee identifier. For a path-call
// like `redis_io::parse_target_repo(...)` the callee_path is the
// fully-qualified path; for a use-imported `parse_target_repo(...)`
// it is the bare name. Equality match against `parse_target_repo`
// covers the bare form; the path form (rare in agentry idioms) would
// require a separate filter — recorded as a follow-up if it appears.
//
// Filters:
// - cs.callee_path = 'parse_target_repo' — restricts to the bare-name
//   call form.
// - caller.is_test = false AND cs.is_test = false — exempt test scopes.
// - No exemption: the function is deleted, no legitimate caller exists.
//   Any match is a reintroduction attempt.
//
// Pre-landing fix list: brief 1 deletes `parse_target_repo` from
// `redis_io.rs:457` and routes the one caller (`fetch_profile` at
// `redis_io.rs:404`) through `TargetRepo::from_str` followed by
// `target_repo.owner()` / `target_repo.repo()` accessors.
//
// Verified zero violations on develop tip after brief 1 migration.
//
// Expected: 0 violations on a clean tree. Any row is a violation.

MATCH (caller:Item)-[:INVOKES_AT]->(cs:CallSite)
WHERE cs.callee_path = 'parse_target_repo'
  AND caller.is_test = false
  AND cs.is_test = false
RETURN caller.qname, caller.crate, cs.file, cs.line
