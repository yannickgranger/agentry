// arch-ban-target-repo-slug-free-fn.cypher — bans direct calls to the
// free function `sanitize_target_repo_slug` outside the canonical
// `TargetRepo::slug` method on the typed routing-key value.
//
// Rationale: slug derivation is the single most security-load-bearing
// transformation of target_repo (it determines the cfdb keyspace name,
// the filesystem path component, and the cache marker name). After
// brief 1 lands the typed `TargetRepo`, the slug derivation lives on
// `TargetRepo::slug()` — collision-resistant via `_` → `__` pre-encoding.
// The free `sanitize_target_repo_slug` function is preserved for
// source-compat (one caller in `anchor_resolver.rs::for_target_repo`
// delegates to `TargetRepo::slug()`); any NEW caller is a violation
// because the free fn accepts `&str`, allowing arbitrary unvalidated
// strings to be slugged as if they were routing keys — exactly the
// cross-target collision risk the typed accessor exists to close.
//
// The cfdb extractor surfaces a `:CallSite` per call expression with
// `callee_path` carrying the textual callee identifier the author
// wrote. For `sanitize_target_repo_slug(...)` (free fn call form),
// `callee_path = 'sanitize_target_repo_slug'`. Exact equality is
// safe — the function name is unique in the agentry workspace.
//
// Filters:
// - cs.callee_path = 'sanitize_target_repo_slug' — restricts to the
//   bare-name call form for the free function.
// - caller.is_test = false AND cs.is_test = false — exempt test scopes
//   per the same convention as arch-ban-unwrap-in-prod.cypher.
// - NOT caller.qname =~ '.*::sanitize_target_repo_slug$' — exempt the
//   function body itself from matching its own forward-compat
//   delegation to TargetRepo::slug.
// - NOT caller.qname =~ '.*::for_target_repo$' — exempt
//   `anchor_resolver::ResolverContext::for_target_repo`, which the
//   bridge function delegates through during the migration window.
// - NOT caller.qname =~ '.*::ensure_target_extracted$' — exempt
//   `intake_validation::ensure_target_extracted`, the second
//   pre-existing legitimate caller surfaced by the brief 1a sweep.
//   Filter by `caller.qname` regex, not by `cs.file` regex, because
//   cfdb path normalization differs across environments (process-exit
//   rule precedent).
//
// Pre-landing fix list (must be in the same brief that lands this rule):
//   None. Brief 1a's migration routes `sanitize_target_repo_slug`
//   through `TargetRepo::slug()` for valid inputs; the two remaining
//   pre-existing callers are exempt by the qname filters above and
//   their migration to direct `TargetRepo::slug()` is scoped into
//   brief 1b.
//
// Verified zero violations on develop tip after brief 1a migration.
//
// Expected: 0 violations on a clean tree. Any row is a violation.

MATCH (caller:Item)-[:INVOKES_AT]->(cs:CallSite)
WHERE cs.callee_path = 'sanitize_target_repo_slug'
  AND caller.is_test = false
  AND cs.is_test = false
  AND NOT caller.qname =~ '.*::sanitize_target_repo_slug$'
  AND NOT caller.qname =~ '.*::for_target_repo$'
  AND NOT caller.qname =~ '.*::ensure_target_extracted$'
RETURN caller.qname, caller.crate, cs.file, cs.line
