# Pre-commit callers gate

The bounded context that owns the *coder's pre-commit orphan-API check*.
Distinct from `reviewer_fence` — that gate runs inside the reviewer
container *after* a commit lands; this one runs inside the coder
container *before* `git commit` and rejects the iteration if a newly
introduced `pub` item has zero in-repo callers and is not listed in the
documented-public-API allowlist.

The runtime helpers live in
`agentry_role_runtime::precommit_gate` and are wired into
`coder_claude_runner` between the optional `dead-pub-check` phase and
the `git commit` phase. The gate is opt-in via the `ra-query` binary's
presence: missing or failing `ra-query` degrades to "skipped" rather
than blocking, so a substrate hiccup does not strand the coder.

## NewPubItem

One newly-introduced `pub` declaration discovered by parsing the unified
diff between `origin/<base>` and `HEAD`. Records the file path, the
new-file line number, the kind (`fn` / `struct` / `enum` / `trait` /
`type` / `const` / `static` / `mod` / `use`), the bare item name (the
identifier passed to `ra-query callers <name>`), and the workspace-stable
fully-qualified name (`<crate>::<bare_name>` for files under
`crates/<dir>/...`, or the bare name otherwise). The FQN is the key the
allowlist matches on, so it stays stable across `dead-pub-check`'s
position-form lookup and the human-edited TOML.

## PublicApiAllowlist

The list of fully-qualified names exempt from the gate's zero-caller
rule. Loaded from `docs/public_api_allowlist.toml` at gate-run time.
Entries are added when a pub item is documented external API or
cross-crate consumed in a way the in-repo caller-graph cannot see (e.g.
items consumed only by downstream binaries or tests outside the
workspace). An empty list rejects every new orphan pub.

## GateDecision

The outcome the gate produces — the runner pattern-matches on this to
decide whether to continue, skip, or emit `done failed` with cause
`pre_commit_callers_gate_failed`:

- `SkippedNoRaQuery` — the `ra-query` binary is absent on PATH; the gate
  is opt-in via its presence and degrades open.
- `SkippedResolverFailed(reason)` — a `ra-query callers` invocation
  failed mid-flight; degrade open rather than false-block.
- `Clean` — every new pub item has at least one caller OR is on the
  allowlist; commit phase proceeds.
- `BlockersFired(findings)` — at least one orphan was found; the runner
  emits the findings and a terminal `done failed`.
