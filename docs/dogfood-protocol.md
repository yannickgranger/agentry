# Dogfood protocol — dispatching briefs into agentry-self-host-v0

This document describes how to plan, dispatch, observe, and respond to
briefs after the cutoff (PR #8 merged on `yg/agentry`). Before the
cutoff, none of this applies — change agentry directly via feature
branches + PRs.

## When it applies

After PR #8 merges on `yg/agentry`, the `agentry-self-host-v0` team is
live and the no-direct-commit rule takes effect. Verify:

```bash
curl -sk https://agency.lab:3000/api/v1/repos/yg/agentry/pulls/8 \
    -H "Authorization: token $(keepassxc-cli show -sa Password --no-password \
        -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx 'gitea/agency.lab')" \
    | jq '{state, merged}'
```

If `merged: true`: you cannot commit to `yg/agentry` directly. Dispatch
a brief.

## The team shape (v0, current)

```
coder-claude-agentry
      │
      ▼
reviewer-mechanical-agentry  (cargo fmt --check / clippy / test — machine truth)
      │
      ▼
reviewer-claude-agentry      (LLM review — scope-guarded; verb-completeness)
      │
      ▼
shipper-agentry              (git push + POST /pulls)
      │
      ▼
ci-watcher-agentry           (polls forge CI; auto-merge on green, fail on red)
```

Sequential scheduler (DAG scheduler in issue #13 will enable parallel
sibling execution). `max_retries=2`; both reviewers list the coder as
their rework-target upstream in `message_graph`. A Blocker finding from
either reviewer rewinds to the coder, bounded by the retry budget.

- **coder-claude-agentry** — workspace is a git worktree off a shared
  bare clone at `auto/<brief_id>`, forked from `base_branch`. Calls
  `claude -p` with the issue body + acceptance. Exitpoint runs `cargo
  fmt --all`, optional `quality-hygiene --fix` if present, then a
  pre-commit self-review (LLM checks verb-completeness on the staged
  diff — emits blocker findings via `emit_finding_model` if any
  declared verb is unapplied), then commits. Does NOT push.
- **reviewer-mechanical-agentry** — read-only workspace, re-runs the
  brief's `acceptance` in isolated `CARGO_TARGET_DIR`. Emits `Shipped`
  on green; `ReworkNeeded` on red with a Blocker tagged `Mechanical{tool,category}`.
- **reviewer-claude-agentry** — read-only workspace, git-diffs against
  `base_branch`, prompts `claude -p` for a JSON array of findings.
  Prompt includes a "Scope guardrail" clause (only flag changes IN the
  diff; pre-existing drift → at most one warn, never block) and a
  "Verb-completeness check" clause (unapplied declared verbs are
  blockers). Emits `Shipped` when claude returns `[]`; `ReworkNeeded`
  per-finding `Model{reviewer_agent_id}` entries when any blocker
  present. Does NOT duplicate mechanical-reviewer scope (fmt/clippy/test).
- **shipper-agentry** — `git push -u origin HEAD:<branch>`,
  `POST /api/v1/repos/.../pulls`. Emits the PR URL, number, and head SHA
  as a Message addressed to `ci-watcher-agentry`. Does not merge.
- **ci-watcher-agentry** — reads the shipper's Message, polls
  `GET /commits/<head_sha>/status` every ~15s. On `state=success` POSTs
  the merge endpoint and emits `Shipped`. On `state=failure|error`
  emits `Failed` with the failing context. On wall-clock timeout the
  daemon tears the container down.

### Reviewer vs CI responsibility

The `reviewer-mechanical-agentry` role runs the per-brief acceptance
command in an isolated `CARGO_TARGET_DIR`. Acceptance SHOULD be limited
to fast, deterministic, per-workspace checks — `cargo fmt --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test
--workspace`.

`scripts/arch-check.sh` is a **CI-only** gate. `cfdb` and `graph-specs`
are *insight tools* used by the main session to author briefs
(`/discover`, `/prescribe`, `specs/concepts/*` drafts) — they inform
architecture decisions before a brief is dispatched. They never belong
in a per-brief container: containers are short-lived and single-purpose,
paying the install cost per brief is wrong, and the tools' role is
analysis rather than enforcement.

CI on the forge has `cfdb` + `graph-specs` pre-warmed. If a brief lands
an arch-check regression, the brief's PR fails CI before merge — that's
the second line of defence.

Reviewer latency matters more than reviewer coverage.

Historical reference: #9 (ci-watcher auto-merge), #10 (bare-clone +
git-worktree), #11 (rework loop), #12 (LLM reviewer) have all shipped.
#13 (DAG scheduler + concurrent briefs) remains open — the current
scheduler is sequential.

## Reviewer design — LLM prompts, not AST parsers

Intent-vs-diff checks belong in claude prompts, not Rust type systems.
The pipeline relies on two layered LLM passes to verify that a brief's
declared verbs all landed:

- **Coder pre-commit self-review** (in `CODER_CLAUDE_AGENTRY_EXITPOINT`
  in `seed.rs`) — before the coder commits, claude inspects the issue
  body and the staged diff. If any declared verb is unapplied, claude
  emits a blocker finding via `emit_finding_model` and the role exits
  `failed`. Cheap pre-filter; first line of defence.
- **Reviewer-claude verb-completeness + scope guardrail** (in
  `REVIEWER_CLAUDE_AGENTRY_SCRIPT` prompt) — the reviewer prompt
  explicitly instructs claude to (a) ONLY flag changes INSIDE the diff
  (pre-existing concerns → at most one warn, never block) and (b) check
  that every CREATE/UPDATE/REPLACE/DELETE/MOVE verb in the task body
  landed in the diff (unapplied verbs are blockers). Backstop.

Both are bash + prompt edits in existing string literals. No Rust type
additions, no AST parsing of verbs, no compile-time enforcement. LLMs
read free-text intent and diffs; that is literally what they are good
at. Reach for prompt engineering before reaching for types.

## Planning a brief

Before writing the payload:

1. **Read the forge issue** (`mcp__forge__forge_get_issue` or via
   browser). Know the acceptance criteria by heart.
2. **Read the affected source files.** Don't rely on the issue body
   alone — it can drift from the code.
3. **Query cfdb** for the affected symbols:
   ```bash
   cd /var/mnt/workspaces/agentry
   cfdb extract --workspace . --db /tmp/cfdb-agentry --keyspace agentry
   cfdb query --db /tmp/cfdb-agentry --keyspace agentry \
       'MATCH (i:Item) WHERE i.qname =~ ".*<symbol>.*" RETURN i.qname, i.kind, i.source_file LIMIT 20'
   ```
4. **Read the relevant spec files**: `specs/concepts/<context>.md` for
   every concept the brief would touch. If the spec is missing a
   concept the brief would add, update the spec in the same brief. If
   the spec disagrees with the code, file a precursor reconciliation
   brief first.
5. **Structure transformations with verbs.** Every diff in the brief's
   description uses one of:
   - `CREATE <crate>:<file>[:<line>]` — new file or new top-of-file item
   - `UPDATE <crate>:<file>:<line>` — edit an existing item
   - `REPLACE <crate>:<file>:<line>` — wholesale swap (e.g. rename + body change)
   - `DELETE <crate>:<file>:<line>` — remove
   - `MOVE <src-path> → <dst-path>` — relocate without semantic change
   
   Free-form instructions ("clean up this module", "refactor as needed")
   are forbidden — they give the coder too much latitude and make the
   mechanical reviewer's acceptance command the only real spec.

## Brief payload

```json
{
  "id": "brf_work_<N>_<short-slug>",
  "project": null,
  "topology": { "name": "agentry-self-host-v0", "version": 1 },
  "payload": {
    "issue_number": <N>,
    "issue_title": "...",
    "issue_body": "CREATE crates/foo/src/bar.rs: pub struct Baz { ... }\nUPDATE crates/foo/src/lib.rs:42: re-export Baz",
    "acceptance": "cargo clippy --workspace -- -D warnings && cargo test --workspace && scripts/arch-check.sh",
    "target_repo": "yg/agentry",
    "base_branch": "develop",
    "pr_title": "feat(<context>): <summary> (closes #<N>)",
    "pr_body": "<verb-structured description + AC + rationale>"
  },
  "budget": {
    "max_wall_seconds": 900
  },
  "escalation": "autonomous",
  "parent_brief": null,
  "submitted_by": "<session-id>",
  "submitted_at": "<iso-8601>"
}
```

`max_wall_seconds` covers the full role-by-role sequential run — coder
(claude prompt + cargo check) + reviewer (clippy + test) + shipper
(push + API call). 900s is a starting budget; tune per brief size.

## Dispatching

```bash
cat > /tmp/brief.json <<EOF
<payload above>
EOF

orchestrator submit /tmp/brief.json
```

Output: `{"submitted": true, "brief_id": "<id>", "stream_id": "..."}`.

## Observing

Trace stream (all events from all roles, ordered):

```bash
redis-cli -p 6380 -a "$(cat ~/.config/agentry/redis.password)" --no-auth-warning \
    XRANGE agentry:brief:<brief_id>:trace - +
```

Live via dashboard: `http://localhost:7800/brief/<brief_id>`.

Verdict:

```bash
redis-cli -p 6380 -a "$(cat ~/.config/agentry/redis.password)" --no-auth-warning \
    XRANGE agentry:verdicts - + | grep <brief_id>
```

Possible outcomes for a `VerdictKind`:

- `Shipped` — team's terminal role emitted shipped. On teams whose
  terminal is `ci-watcher-agentry`, this means the PR was also
  auto-merged on CI green; check the ci-watcher container's final
  event payload for `{merged:true, pr_url, pr_number}`.
- `Failed` — a role emitted failed and no rework budget remains (or
  the role has no upstream to rewind to). Read the trace stream +
  orchestratord log (`/tmp/agentry-orchestratord.log`) to diagnose.
- `ReworkNeeded` — intermediate state, not a terminal team outcome.
  Visible in the trace for reviewer-mechanical or reviewer-claude when
  a Blocker finding fires. The daemon rewinds to the coder with the
  finding payload in `TeamContext.messages`. If retry budget exhausts,
  the team resolves `Failed` with reason "rework requested but retry
  budget exhausted".
- `PermitViolation` — a role emitted a `tool_call` outside its
  `tool_allowlist` / `permit_scope`. Permit broker killed the
  container. Read `agentry:brief:<brief_id>:audit` for what was
  attempted.

## Responding to failures

- **Coder couldn't finish within `max_wall_seconds`.** Raise the budget
  and resubmit. If past ~1800s, break the task into smaller briefs.
- **Coder self-review emitted `failed` with unapplied-verb findings.**
  Read the findings in the verdict reason — claude-pre-commit caught
  that the diff was incomplete. File a finisher brief that targets ONLY
  the missed verbs.
- **Reviewer rejected (mechanical — fmt/clippy/test failure).** Read
  the last ~50 lines in the verdict reason. Either fix the issue_body's
  verb guidance or adjust the acceptance command.
- **Reviewer rejected (claude — design/clarity/invariant blocker).**
  Read per-finding `Model{reviewer_agent_id}` entries in the trace.
  Rework loop rewinds to coder for up to `team.max_retries=2` retries;
  exhaustion resolves Failed. If the blocker is out-of-scope (a
  pre-existing concern the scope-guardrail clause failed to bound),
  file that as a follow-up and re-dispatch the original intent in a
  more tightly-scoped brief.
- **Shipper failed to push.** Typically base moved. Resubmit; worktree
  allocates off current base_branch.
- **ci-watcher red.** Forge CI failed. Open the PR on the forge, read
  the failing check log, file a follow-up brief with the fix. The
  failed PR stays open until a superseding brief merges a fix (or the
  human closes it).
- **ci-watcher green → auto-merged.** Nothing to do. The next brief can
  dispatch immediately; the host bare clone auto-refreshes via the
  fetch refspec set by `ensure_bare_clone`.

## Cleaning up

A failed brief retains its workspace at
`/var/mnt/workspaces/agentry-work/briefs/<brief_id>/` for audit. After
diagnosing, remove it with:

```bash
rm -rf /var/mnt/workspaces/agentry-work/briefs/<brief_id>/
```

(A dedicated `orchestrator prune` command is planned; not yet built.)

Successful briefs destroy their workspace automatically on the team's
final `Shipped` verdict.

## Post-merge verify checklist

When a brief's PR merges on the forge, run this sequence BEFORE
dispatching the next brief. Skipping steps silently regresses the
pipeline (reseed-with-stale-binary is the classic pitfall).

```
# 1. Pull develop on the host clone.
cd /var/home/yg/workspaces/agentry && git pull --ff-only origin develop

# 2. Rebuild orchestratord + orchestrator CLI.
cargo build --release --bin orchestrator --bin orchestratord

# 3. Restart the daemon with full env (redis auth, gitea token from
#    keepassxc, webhook secret). A token-missing daemon silently
#    rejects briefs with "GITEA_TOKEN not in daemon env".
pkill -f orchestratord
TOKEN=$(keepassxc-cli show -k ~/agent-zero/key/claude.key --no-password \
    -a Password ~/agent-zero/claude.kdbx gitea/agency.lab)
# (start script — see repo runbook; key env: AGENTRY_REDIS__URL,
#  AGENTRY_REDIS_PASSWORD, AGENTRY_WEBHOOK__SECRET, GITEA_TOKEN)

# 4. Reseed Redis so the new seed.rs content is live.
./target/release/orchestrator seed

# 5. Verify the new content is in Redis (not just compiled into the binary).
redis-cli -p 6380 -a "$(cat ~/.config/agentry/redis.password)" \
    --no-auth-warning GET agentry:role:reviewer-claude-agentry:v1 \
  | grep -oE "<expected clause snippet>"
```

The bare clone's `refs/heads/*` refresh automatically now (the fetch
refspec is set on clone by `ensure_bare_clone`). If you ever hit
"no changes produced" on a prose-only brief, diagnose the bare clone:
`git rev-parse refs/heads/develop` vs `cat FETCH_HEAD` — divergence
means the refspec is missing (shouldn't happen on clones created after
the `ensure_bare_clone` fix merged).

## Post-cutoff reality checks

Before submitting any brief:

- `scripts/arch-check.sh` passes on the current develop tip.
- `just agentry-net-up` + `just dev-redis-up` both idempotent and clean.
- `orchestrator seed` has been run against the current build (so the
  registry has the latest role definitions).
- `orchestratord` is running and has loaded the signing key.

After submitting: watch the trace stream until a verdict lands. Don't
walk away from a brief before it terminates — a stuck brief should
itself become a bug report (either raise the budget, or investigate
why a role didn't respect the wall-clock timeout).
