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

## PlannerPayload

The parsed, role-local view of a meta-brief startup bundle as the planner
runner consumes it: `brief_id`, `intent`, `success_criteria`,
`child_topology` (default `agentry-self-host-v0`), `max_children` (default
10), `base_branch` (default `develop`), `target_repo` (default
`yg/agentry`). Built by `parse_planner_payload` once at the top of the
runner's `main` so subsequent prompt assembly, response capping, and
child-brief construction share the same materialised context rather than
re-walking JSON pointers per call site. Defaults mirror the bash
`jq -r '... // "..."'` fall-throughs of the legacy
`PLANNER_CLAUDE_AGENTRY_SCRIPT` bash heredoc.

## RebaserPayload

The parsed, role-local view of a chained rebaser brief as the
pr-rebaser-runner consumes it: `target_repo` (default `yg/agentry`),
`pr_number`, `branch` (required), `base_branch` (default `develop`),
`forge_host` (default `agency.lab:3000`, but in normal operation arrives
populated via the daemon's phase-3 cascade through `agentry.toml [forge]
default_host`). Built by `parse_rebaser_payload` once at the top of the
runner's `main` so subsequent fetch / checkout / rebase / push commands
share the same materialised context. Defaults mirror the bash
`jq -r '... // "..."'` fall-throughs of the legacy
`PR_REBASER_AGENTRY_SCRIPT` bash heredoc, with the inline
`agency.lab:3000` literal eliminated in favor of the daemon-injected
value.

## PayloadError

Reason a `parse_rebaser_payload` call rejected a startup bundle. Today
the only variant is `MissingBranch` (an absent or empty
`brief.payload.branch` cannot be rebased). The runner maps each variant
to an error event and `done failed` so misrouted briefs surface a
diagnostic instead of silently no-op'ing. Sibling type to
`SelfReviewResult` and `BriefContext` — a structured, exhaustive error
domain rather than `String`.

## RebaseOutcome

Tri-state classification of a `git rebase origin/<base>` invocation:
`Success` when the rebase exit code is 0; `Conflict` when the exit is
non-zero AND `git status --porcelain=v2 -uno` reports unmerged paths
(human-resolvable merge conflicts surfaced as findings); `Fatal` when
the exit is non-zero with no unmerged paths (substrate failure such as
a missing ref or detached worktree). The pr-rebaser-runner's
`classify_rebase` returns this so the success / push and conflict /
abort branches stay separately testable from the I/O around them.

## PrePushRebaseDecision

Tri-state classification of the shipper-runner's pre-push `git rebase
FETCH_HEAD` invocation: `Proceed` when the rebase exit code is 0 (push
step is reached); `AbortConflict` when the exit is non-zero AND
`git status --porcelain` reports unmerged paths (the coder branch has
diverged from the freshly-fetched base unresolvably); `AbortFatal` when
the exit is non-zero with no unmerged paths (substrate failure such as a
missing FETCH_HEAD or git spawn error). `classify_pre_push_rebase`
returns this so the proceed / abort branches stay separately testable
from the I/O around them. Pre-push fetch + rebase closes the
develop-advances-during-coder-run race window that would otherwise
produce a stale-base PR with `mergeable=false` once the forge recomputes
mergeability against the current develop tip; sibling fix to
ci-watcher's chained pr-rebaser fallback for the
develop-advances-during-CI-poll case.

## ShipperPayload

The parsed, role-local view of a brief startup bundle as the shipper
runner consumes it: `brief_id`, `target_repo` (default `yg/agentry`),
`base_branch` (default `develop`), `pr_title` (default `auto(<brief_id>)`),
`pr_body` (default `Agentry-produced PR. See brief trace stream.`), and
`forge_host` (default `agency.lab:3000`). Built by `parse_shipper_payload`
once at the top of the runner's `main` so the subsequent push, PR-create,
and ci-watcher hand-off share one materialised context rather than
re-walking JSON pointers per call site. Defaults mirror the bash
`jq -r '... // "..."'` fall-throughs of the legacy
`SHIPPER_AGENTRY_SCRIPT` bash heredoc.

## PrCreateResponse

Parsed shape of the forge `POST /repos/.../pulls` response — the bits the
shipper hands off to `ci-watcher-agentry` via `emit_message`: `pr_number`
and `pr_url`. `parse_pr_response` returns `None` when `html_url` is
missing or empty (the bash `[ -z "$pr_url" ] || [ "$pr_url" = "null" ]`
failure check), which the runner treats as a fatal `done failed` so a
malformed forge response cannot ship a brief that has no PR to watch.

## AttemptResult

The outcome of one merge-POST attempt as the ci-watcher's retry loop
classifies it: `Ok(http_code, body)` when curl returned an HTTP code,
or `Err(detail)` when the curl invocation itself failed before any
code was received. Sibling input type to `MergeRetryOutcome` — the
caller-supplied `do_post` closure into `run_merge_retry_loop` returns
one per attempt, decoupling the loop's decision logic from the actual
HTTP transport so the test crate exercises the loop without a live
forge.

## MergeRetryOutcome

Terminal classification the ci-watcher's `run_merge_retry_loop` reaches
after exhausting attempts or hitting a definitive HTTP code. `Merged
{ attempt }` maps to `done shipped`. `NonTransientFail { code, detail,
attempt }` maps to `done failed` — these are real merge errors
(authorization, malformed body, server fault) that the rebaser cannot
fix. `ExhaustedTransient { code, detail }` is the dominant
concurrent-dispatch race: persistent 405/409 from the merge POST
because `develop` advanced between the GET-mergeability check and the
POST attempt. The runner routes that variant through the same
`chain_trigger_pr_rebaser` helper the GET-side `mergeable=false`
branch uses, eliminating the manual-rebase requirement for the
post-merge-POST race.
