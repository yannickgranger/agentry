# Agent contract

The bounded context that owns *what a container says on stdout*. Every line
an agent emits is parsed as one `Event`; every event is mirrored to the
brief's trace stream; the terminal event is the one that carries an
`EventVerdict`. This is Published Language between `execution` and every
containerised agent, regardless of substrate or language.

## EventVerdict

Terminal outcome an agent declares in its own stdout: shipped, failed, or
escalated. Distinct from the team-level `outcome::Verdict` — this one
travels on the NDJSON wire; the team-level one is persisted as the brief's
verdict. Kept separate so the agent's self-report cannot silently overwrite
the daemon's reasoned conclusion.

## ToolCall

A tool-invocation attempt emitted by the agent: the tool name and a JSON
args payload. The permit broker checks each one against the permit's
`ToolAllowlist` and, for filesystem writes, against the permit's
`PermitScope`.

## EventKind

The sum type over all event shapes the agent may emit: freeform event,
tool call attempt, inter-role message, log line, done/terminal. Serialised
with a `type` discriminator tag so each NDJSON line is self-describing.

A `status` variant carries a watchdog-emitted diagnosis: the agent id,
the selector that matched it, an `ok`/`stuck` boolean pair, the
diagnostician's natural-language reason, and the trace event ids that
backed the judgment. Watchdog ticks XADD Status events to the agent's
brief trace stream so projector watermarks advance consistently and
downstream consumers (dashboards, captain stdin-daemon, future
commandant officer council) read them on the same wire as every other
agent event.

## DoneReason

Optional structured cause attached to a terminal `Done` event when the
verdict was forced by an unexpected exit, a timeout, or an external signal,
rather than by the role's happy path. Carries a short symbolic `cause`
("unexpected_exit", future: "timeout", "signal") and an optional
`exit_code` if known at the emit site. Absent on roles that called
`emit_done` explicitly with a normal verdict.

## DoneGuard

Drop guard owned by every Rust role binary. Constructed at the top of
`main`. On normal exit the role calls `emit_done(verdict, ...)`, the guard
sees a flag and no-ops on drop. On panic / `?`-bubbled error / abrupt
return, the flag is unset and `Drop` synthesises `done failed` with
`reason: { cause: "unexpected_exit", exit_code: None }`. Closes the
silent-exit failure class structurally — the substrate always sees a
terminal `Done` event for every spawned role, and the daemon never has to
synthesise generic "agent exited without done event" verdicts.

The `BASH_PRELUDE` `EXIT` trap (legacy bash heredoc roles) does the same
job for roles that haven't migrated yet; both patterns coexist until EPIC
\#161 ports every role to Rust.

## PrOpened

Result of a successful gitea pull-request open call. Carries the numeric
`pr_number` and the public `pr_url`. Returned by the git-operator family
(`git-op-push`, the legacy combined `git-operator`) so the binary can
attach both fields to its terminal `done shipped` event without re-parsing
the gitea response at the call site.

## StreamErr

Failure type returned by the `stream_claude` lib helper that Rust role
binaries use to invoke `claude -p --output-format stream-json --verbose`.
Distinguishes the two failure modes the daemon's projector parses
differently:

- `ClaudeFailed { exit_code, detail }` — `timeout(1) claude -p ...`
  exited non-zero (or the spawn itself failed). `exit_code` is `124` for
  wall-clock timeouts, `127` for command-not-found, the child's actual
  code otherwise; `-1` reserved for spawn / wait failures. `detail`
  carries the tail of stderr (≤4 KiB).
- `TranscriptEmpty { path }` — the child reported success, but the
  transcript bind-mount could not be written (rootless podman subuid
  mismatch on `/transcripts` is the dominant cause). Surfaced explicitly
  so the operator sees the real failure mode rather than a downstream
  parse error.

Both variants are wire-compatible with the bash `stream_claude` helper's
emit shape. Roles that catch a `StreamErr` emit a degradation event and
either `done failed` (hard error) or — for soft-fail call sites like the
coder exitpoint's self-review — degrade to a permissive default and
proceed.

## Event

A timestamped event: `Ts` + `EventKind`. The unit that the spawner reads
off stdout, writes to the trace stream, and (for `ToolCall`) audits.

## Provider

Which AC verifier provider an `ac-verifier-runner` invocation runs:
`Claude`, `Gemini`, or `Grok`. Each variant maps 1:1 to a bind-mounted host
binary on `PATH` inside the container (`ac-verifier`, `ac-verifier-gemini`,
`ac-verifier-grok`) — the strings the bash port's `command -v` pre-flight
checks looked for, kept wire-compatible by `binary_name`. `parse(&str)`
accepts the lowercase `--provider` flag value and returns `None` for
anything outside the closed set, surfaced by the binary as a "bad
--provider flag" degradation event.

## PriorFinding

A reviewer-emitted blocker harvested from the bundle's
`team_context.messages[].payload.findings[]` and reshaped into the small
triple the coder runner needs at prompt-build time: `message`,
`prohibitions`, `requirements`. Constructed by `collect_blocker_findings`
and consumed by `build_rework_banner` to compose the rework injection
banner that prefaces a re-iteration of the brief. Strictly an internal
shape — no on-wire form, no daemon-side counterpart.

## BriefContext

The parsed, role-local view of a startup bundle as the coder runner
consumes it: `brief_id`, `base_branch`, `issue_title`, `issue_body`,
`acceptance`, derived `branch` (`auto/<brief_id>`), `topology_name`,
prebuilt `rework_banner`, the harvested `blocker_findings`, and
`allowed_tools` propagated from the permit. Built by
`parse_brief_context` once at the top of `run()` so subsequent prompt
assembly, topology gating, and exitpoint-phase steps share the same
materialised context rather than re-walking JSON pointers per call site.

## SelfReviewResult

Parsed shape of the coder's optional self-review claude reply
(`{all_applied, unapplied}`). `parse_self_review_object` strips fences,
slices between the first `{` and last `}`, and decodes the object —
returning `None` when the reply is not a parseable JSON object so the
caller can apply the soft-fail tolerance (degrade to `all_applied: true`
and proceed). When `all_applied: false`, the runner emits one
`completeness` blocker per entry in `unapplied` and `done failed` with
cause `self_review_unapplied`.
