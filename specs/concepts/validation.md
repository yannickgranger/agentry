# Validation

The bounded context that owns *deterministic gates run against a candidate
diff before the daemon accepts a brief*. Distinct from `review` —
`ReviewFinding` is the structured record of *any* gate (mechanical or
model); `Validation` is the pipeline of mechanical gates wired per
`BriefKind` and run by the ship tool.

Brief 2 of EPIC #152 introduces the type-level scaffolding only. Stub
implementations return a passing report; brief 3 ports the existing
reviewer-mechanical logic into real `Validator` impls; brief 4 wires the
ship binary to dispatch via `validators::registry_for(brief.kind)`.

## Validator

The trait every gate implements. Object-safe, async, fallible. Each impl
declares a stable `name()` (used in dashboards and reports) and a `run()`
that consumes a `BriefCtx` and returns a `ValidatorReport`. The trait is
intentionally agnostic of run mechanism — implementations may shell out
to cargo, hit a container, walk the diff in-process, or call a remote
service. The pipeline holds `&'static dyn Validator`s — every concrete
implementation is a unit-struct singleton, addressable by static
reference.

## BriefCtx

The per-brief execution context handed to every validator: the workspace
path to inspect, the brief id (for log + report attribution), and the
list of files changed by the coder vs the base branch. The ship tool
populates `changed_files` from `git diff --name-only`; stubs and tests
leave it empty.

## ValidatorReport

The record a validator produces. Names the validator that produced it,
records pass/fail, and carries zero or more `Finding`s. Pass means no
findings worth surfacing; fail means at least one blocker — the daemon
treats a failing validator pipeline as `VerdictKind::ReworkNeeded` and
routes findings back to the upstream coder via the team's
`message_graph`. Constructors `pass(name)` and `fail(name, findings)`
keep the happy path concise.

## FmtCheck

The `cargo fmt --check` gate. Runs in the brief's per-brief
`CARGO_TARGET_DIR` so it doesn't clobber the coder's `target/`. Pass on
exit 0; on non-zero exit emits a single Blocker `Finding` whose message
is the 2 KiB tail of stderr followed by the 2 KiB tail of stdout
(matching the reviewer-mechanical bash script's combined-output cap).

## ClippyWorkspace

The full-workspace clippy gate: `cargo clippy --workspace --all-targets
-- -D warnings`. Same single-Blocker shape as `FmtCheck` on failure.
Used by pipelines for brief kinds whose blast radius is the whole
workspace (`Refactor`, `NewFeature`, `Substrate`).

## ClippyScoped

The diff-scoped clippy gate. Walks each path in `BriefCtx::changed_files`
up the directory tree until it hits a `Cargo.toml` containing
`[package]`, collects the unique crate names, and runs `cargo clippy -p
<crate>` per crate. Aggregates one `Finding` per failed crate. Falls
back to the workspace-wide invocation when `changed_files` is empty
(e.g. before brief 4 populates it). Used by `BriefKind::Mechanical`,
where lint scope should match the diff.

## TestWorkspace

The `cargo test --workspace` gate. Same per-brief target dir + single-
Blocker-on-failure shape as `FmtCheck`.

## ArchCheck

The `bash scripts/arch-check.sh` gate — agentry's spec-equivalence and
cfdb-ban-rule pipeline. The script is agentry-specific, so the
validator no-ops with a passing report (no findings) when the script is
absent: the same ship binary runs validators in projects that don't use
arch-check.

## DeadPubCheck

The pre-commit dead-pub gate, ported into the validator pipeline. Pipes
`{diff, workspace_root}` JSON to the bind-mounted
`/usr/local/bin/dead-pub-check` binary (whose protocol is documented in
`crates/coder-precommit/src/bin/dead_pub_check.rs`). Falls through with
a passing report and a `debug!` log when the binary isn't present —
mirrors the `dead_pub_check_unavailable` warn-skip in the existing
reviewer-mechanical seed script.

## AddedPubItem

One newly-added `pub` declaration extracted from a unified-diff hunk by
the dead-pub-check parser. Records the file path, the new-file line
number, the kind (`fn` / `struct` / `enum` / `trait` / `type` / `const`
/ `static`), and the bare item name. The parser
(`coder_precommit::parse_added_pub_items`) lives in lib so the
[[bin]] target and integration tests share a single implementation.

Workflow validation is a separate pipeline that runs against
`TeamTopology` records, not against a coder's diff. Six checks compose
the union: vocabulary integrity (parse-time, structurally enforced by
`#[serde(deny_unknown_fields)]` on the workflow types — typos and
unknown keys are rejected before any runtime check sees the topology),
type integrity (non-zero version, non-empty name / roles / terminal),
reference integrity (every `roles[]` entry resolves in the role
registry, every `message_graph` endpoint is in `roles[]`, the
`terminal_role` is in `roles[]`), topological integrity (at least one
entry role, the terminal is reachable from some entry, no orphans),
acyclicity, and single-terminal (exactly one role with no outbound
edges, equal to the declared `terminal_role`). The validator collects
across all checks without short-circuit — see `team_validator.rs`.
Workflow validation runs at two trigger points: register-time (the
`orchestrator team register` CLI runs it before persisting and rejects
the body on any violation) AND dispatch-time (the daemon's
`handle_brief` runs it between `fetch_team` and the role spawn loop, so
a stale or malformed catalog entry is caught with a structured
`team_validation_failed` trace event before any container fires).
