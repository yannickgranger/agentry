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

## The team shape (v0)

```
coder-claude-agentry  →  reviewer-mechanical-agentry  →  shipper-agentry
```

Strict sequential (`max_retries=0`). No rework loop. No ci-watcher.
Human reviews + merges the produced PR.

- **coder-claude-agentry** — clones `target_repo` at `base_branch`,
  creates `auto/<brief_id>`, calls `claude -p` with the issue body +
  acceptance command + workspace path, runs the acceptance command,
  `git add -A && git commit`. Does NOT push.
- **reviewer-mechanical-agentry** — reads the coder's workspace
  read-only, re-runs the acceptance command in isolation (`cargo clippy
  -D warnings && cargo test`). Emits `Shipped` if green, `Failed`
  otherwise. No LLM.
- **shipper-agentry** — `git push -u origin HEAD:<branch>`,
  `POST /api/v1/repos/.../pulls`. Emits `Shipped` with the PR URL.

Issues #9–#13 upgrade this team: #9 auto-merges on CI green, #10 uses
bare-clone + git-worktree, #11 adds the rework loop, #12 adds an LLM
reviewer, #13 makes the scheduler a DAG with concurrent briefs.

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

Three possible outcomes for a `VerdictKind`:

- `Shipped` — shipper opened a PR. Pull `pr_url` from the shipper's
  final event payload. Review + merge on the forge.
- `Failed` — coder couldn't complete, reviewer rejected, or a role
  exceeded `max_wall_seconds`. Read the trace stream + orchestratord
  log (`/tmp/agentry-orchestratord.log`) to diagnose.
- `PermitViolation` — a role emitted a `tool_call` outside its
  `tool_allowlist` / `permit_scope`. Permit broker killed the
  container. Read `agentry:brief:<brief_id>:audit` for what was
  attempted.

## Responding to failures

- **Claude couldn't finish the task within `max_wall_seconds`.** Raise
  the budget and resubmit. If raising the budget past ~1800s, break the
  task into smaller briefs.
- **Reviewer rejected (clippy / test failure).** Read the last ~50
  lines of the failure in the reviewer's verdict reason. Write a new
  brief with a more precise `issue_body` — e.g. quote the failing
  clippy lint name and tell the coder to respect it.
- **Shipper failed to push (`git push` rejected because base moved).**
  Someone merged on develop between brief start and end. Resubmit; the
  coder re-clones from the current develop.
- **Shipper opened a PR but the human reviewer rejected it.** Comment
  the rejection reasons on the PR; close it; file a follow-up brief
  incorporating the feedback. The next brief cites the closed PR's
  number in `issue_body` for context.

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
