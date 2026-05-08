# Anchor resolver

The anchor resolver resolves `orchestrator_types::contract::AssertionAnchor`
values against the local agentry workspace's cfdb keyspace and
`specs/concepts/` directory. It is a pure helper module: B6a lands the
resolver and its tests in isolation; B6b wires it into daemon intake so
the council's "every brief carries verifiable anchors" intent becomes a
runtime check, not just a documentation expectation.

The dispatch in `resolve_assertion` matches `AssertionAnchor` exhaustively
without a wildcard arm. Adding a new `AssertionAnchor` variant therefore
forces a deliberate resolver decision at the type-system level, not a
silent fall-through.

The cfdb path categorizes the outcome of `cfdb query` into three explicit
cases so a reviewer can tell spawn-failure apart from benign empty
results: spawn failed; process exited and stdout was empty (likely a
spawn or db-config error masquerading as success); process exited and
stdout was non-empty (parse the JSON regardless of exit status, because
cfdb is known to exit non-zero with `EmptyResult` warnings on
well-formed empty queries — `scripts/arch-check.sh` documents the same
behavior). The qname-injection guard rejects any qname containing a
double-quote character before constructing the Cypher string.

The spec-concept path is contractual, not fuzzy: it requires exact
case-insensitive ASCII equality between the requested section and an
ATX-style heading in the file. Path inputs are guarded against absolute
paths and parent-directory traversal before any filesystem read.

## ResolverContext

The lookup environment for the resolver. Carries the cfdb database path
and keyspace name used when shelling out to `cfdb query`, plus the
workspace-relative `specs/concepts/` directory used as the root for
spec-concept anchor lookups. Public so callers (B6b daemon intake, and
tests) can construct fixture contexts without daemon wiring.

## AnchorResolution

The outcome of resolving a single anchor. `Resolved` means the anchor
was located in the source of truth (cfdb keyspace or spec file). 
`NotFound` carries a human-readable `reason` string explaining why
resolution failed; the cfdb path produces distinct reasons for spawn
failure, empty stdout, JSON parse failure, missing rows array, and an
empty rows array, so a reviewer reading a verdict can tell the failure
modes apart.
