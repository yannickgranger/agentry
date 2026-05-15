# Intake validation

The bounded context that owns *the brief-level checks the daemon runs
between accepting a brief off Redis and dispatching it to a team*. Today
the surface is the contract validator (`validate_brief_contract` /
`validate_brief_contract_for_target`) and the per-target_repo extraction
helper that populates the cfdb keyspace + specs cache the contract
validator looks up against.

The contract validator iterates a brief's assertion anchors and resolves
each against a `ResolverContext` (see `anchor_resolver.md`). Before F1c
the daemon assumed every cfdb keyspace had been pre-extracted out of
band; with `ensure_target_extracted` the daemon can populate a fresh
target_repo's keyspace on first intake and serve subsequent briefs from
cache.

## EnsureExtractedRequest

Inputs to `ensure_target_extracted`. Carries the `target_repo` slug
source string (sanitized to a filesystem-safe slug at call time), the
brief's `head_sha` (used as the cache marker — re-extraction triggers
when the marker is absent or differs), the `clone_url` (derived by the
daemon from the brief at call time, never stored in config), and the
`work_root` under which `<work_root>/cfdb/<slug>` and
`<work_root>/specs/<slug>` live. F1c.tight (future) will extend the
shape to support explicit-sha checkout via `git fetch + git checkout`;
V1 ships with default-branch-HEAD semantics for the underlying
shallow clone.

## IntakeError

Brief 1b lands two pre-mint intake gates: the daemon admits a brief only
when its `payload.target_repo` parses through `TargetRepo::from_str` and
the parsed `owner` is in `cfg.forge.allowed_owners`. Both gates raise an
`IntakeError` variant — `MissingTargetRepo` for the absent / malformed
case (closes the URL-fragment injection vector by ensuring the daemon
never composes a clone URL from an unvalidated string) and
`OwnerNotAllowed { owner }` for a target_repo whose owner failed the
allowlist intersection. The daemon emits a `Failed` verdict plus a
`BriefRejected` trace event for each rejection and the brief is not
spawned. There is no permissive fallback — no `_unknown` keyspace, no
inline byte-map salvage. Permit-broker `forge:write` enforcement remains
the downstream defence-in-depth gate; `IntakeError` is the pre-mint
gate.

## EnsureExtractedOutcome

The outcome of `ensure_target_extracted`. `CacheHit` means the
`<slug>.head_sha` marker matched the requested sha and no extraction
was needed. `Extracted { items }` means a fresh shallow clone + cfdb
extract + specs copy succeeded; `items` is best-effort node count read
back from the keyspace JSON (`0` is non-fatal when parsing fails — the
keyspace exists, only reporting did not). `Failed { reason }` carries a
human-readable cause when any step in the pipeline (tempdir, git
clone, cfdb spawn / extract, specs copy, marker write) fails — the
daemon treats it as an intake failure but does not crash. Operators
invalidate the cache by removing the `<slug>.head_sha` marker; the next
call re-extracts.
