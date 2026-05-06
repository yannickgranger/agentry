# Allowed tools

> Status: **ratified**. Code landing PR: #233b-1. The type is wired into
> `AgentRole.allowed_tools` and propagated through `mint_permit` into
> `WorkPermit.allowed_tools`. The `stream_claude` consumer (typed
> `emit_tool_refused` + `parse_tool_refusal`, lifting `parse_allowed_tools`)
> lands in #247 and remains gated by `refusal.md`'s draft status.

The bounded context that owns *pre-spawn tool fencing for Claude-CLI roles*.
A role spawns the `claude` binary with `--allowedTools <pattern>...`; the
patterns name Claude tools (`Bash(cargo fmt:*)`, `Read`, `Edit(*.rs)`) and
follow Claude's own pattern grammar. The fence runs inside the Claude
process before any tool can fire — violations never reach the daemon.

This is intentionally distinct from the daemon-side `ToolAllowlist` in the
`permits` context: that one carries exact-match symbolic names (`bash`,
`read`, `edit`) and is enforced post-hoc by the permit broker against
`tool_call` events. The two grammars live in two different value domains
and are NOT auto-synchronised — they enforce at different layers, against
different vocabularies, with different failure shapes (Claude refusal vs.
daemon kill).

## AllowedTools

The list of `claude --allowedTools` pattern strings a role declares for its
spawned Claude process. Open-ended grammar — patterns are passed through to
the Claude CLI verbatim. Carried as a `Vec<String>` newtype with serde
round-trip; consumers (role spec field, `stream_claude` arg-builder,
refusal observer) land in #233 wiring.

## Check

One entry in `quality-fast`'s JSON report. Carries the check `name`
(e.g. `cargo-check[quality-fast]`), an `ok` boolean, and the captured
`stdout` / `stderr` from the underlying CLI. The constructor
`Check::skipped(name, reason)` builds an `ok = true` record with
`stderr` set to the reason, used when a tool is not on PATH or the
input scope is empty (`changed_crates` empty → the
`Check::skipped("cargo-check", "no changed Rust crates")` placeholder
keeps the report shape informative).

`quality-fast` is the SOLE compile-feedback path the coder role invokes
under its `Bash(quality-fast:*)` fence. After Brief 237a, its check
inventory is: `cargo-fmt[<crate>]` (`cargo fmt --check -p <crate>` per
changed crate), `cargo-check[<crate>]` (`cargo check -p <crate>
--all-targets` per changed crate), `cargo-clippy[<crate>]` (`cargo
clippy -p <crate> --all-targets -- -D warnings` per changed crate),
`cfdb[<query>]` (pre-paid scoped Cypher queries against the cfdb
index, scoped via `--files` to the changed file set),
`ra-query-pub-surface[<crate>]` (pub-surface query per changed crate),
and `arch-check` (`bash scripts/arch-check.sh`, whole-repo invariant).

Scope is **changed crates only** (`git diff --name-only HEAD` →
`derive_changed_crates`). There is no `--workspace` escape hatch:
workspace-wide `cargo check` / `cargo clippy` / `cargo test` runs only
at the substrate validators downstream (see `validation.md`). Design
intent is that the coder role uses `quality-fast` for ALL compile
feedback, with the heavier whole-workspace pass deferred to the slow
tier so the inner-loop signal stays scoped to what the coder actually
touched.
