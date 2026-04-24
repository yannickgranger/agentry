# Project rules for Claude on agentry

Project-local rules. Complements `~/.claude/CLAUDE.md` (global) and
`specs/dialect.md` (the graph-specs contract). Read before any work.

## The cutoff

Once issue #8 on `yg/agentry` merges, agentry is built by agentry. No
Claude-authored `git push` or PR on `yg/agentry` after that point. Every
change is a brief dispatched into the `agentry-self-host-v0` team.

**No break-glass exception.** If agentry breaks such that no brief
completes, root-cause it from the trace stream + orchestratord log, then
convince the user case-by-case — do not author a direct fix unless
explicitly approved for that one incident.

Before any work on `yg/agentry`, check PR #8 status on the forge:

```bash
curl -sk https://agency.lab:3000/api/v1/repos/yg/agentry/pulls/8 \
    -H "Authorization: token <gitea-token>" | jq '{state, merged}'
```

If `merged: true` — no direct commits. Dispatch a brief. See
`docs/dogfood-protocol.md`.

## Terminal rules

- Claude agents use the host `claude` CLI (Claude Max subscription),
  never the per-token Anthropic API. Grok and Gemini APIs are fine.
- Dev Redis is always `127.0.0.1:6380` on local podman. Never LXC 401
  (`.152`) or 522 (`.189`).
- Every change: feature branch → PR vs `develop` → CI green → merge.
  Never bypass hooks, never push to `develop`/`main` directly.
- Rebase stacked PRs onto `develop` the instant the base PR is
  approved for merge. Merging a stacked PR after its base has already
  merged orphans the feature branch and the commits never reach
  `develop` (see PR #4 and #18 recoveries).

## Process rules

- When user questions a choice: answer WHY first, wait for redirection.
  Do not reverse-pitch.
- When unsure: "I don't know, stopping." No pitched alternatives.
- Factual commits. No milestone numbering (no M0, M1, etc.). No
  celebratory language.
- Don't use `TaskCreate`/`TaskUpdate`. Don't write memory files under
  `~/.claude/projects/.../memory/`. Session handoff is
  `mcp__memory__save_state` + `mcp__memory__set`.

## Methodology

Every pub-surface change updates `specs/concepts/*.md` in the same PR.
Enforced by `scripts/arch-check.sh` and CI (`.gitea/workflows/arch.yml`).

Ban rules land one per PR:
- Each new `.cfdb/queries/*.cypher` comes with a justification and
  zero existing violations. Fix existing violations in the same PR.
- No baseline file, no ratchet, no allowlist.

## Brief discipline

Before dispatching any brief into `agentry-self-host-v0`:

1. Read the forge issue body.
2. Read the affected source files (not just the issue text).
3. Query cfdb for the affected symbols / call sites:
   ```bash
   cfdb extract --workspace <repo> --db /tmp/cfdb --keyspace agentry
   cfdb query --db /tmp/cfdb --keyspace agentry \
       'MATCH (i:Item) WHERE i.qname =~ ".*<symbol>.*" RETURN i.qname, i.kind'
   ```
4. Read the relevant `specs/concepts/<context>.md`. If the spec is
   missing or stale for what the brief would touch, the spec update
   goes in a precursor brief — or is folded into the brief scope.

Brief payload uses verbs:

```
CREATE | DELETE | REPLACE | UPDATE | MOVE  <crate>:<file>:<line>
```

Free-form "fix this issue" briefs are forbidden. Every transformation
is explicit and verifiable.

Example skeleton:

```json
{
  "id": "brf_work_<N>_<slug>",
  "project": null,
  "topology": { "name": "agentry-self-host-v0", "version": 1 },
  "payload": {
    "issue_number": <N>,
    "issue_title": "...",
    "issue_body": "CREATE <file:line>: ...\nUPDATE <file:line>: ...",
    "acceptance": "cargo clippy --workspace -- -D warnings && cargo test --workspace && scripts/arch-check.sh",
    "target_repo": "yg/agentry",
    "base_branch": "develop",
    "pr_title": "feat(<ctx>): <summary> (closes #<N>)",
    "pr_body": "<full description with verbs + AC>"
  },
  "budget": { "max_wall_seconds": 900 },
  "escalation": "autonomous",
  "parent_brief": null,
  "submitted_by": "<session-id>",
  "submitted_at": "<iso-8601>"
}
```

See `docs/dogfood-protocol.md` for the full dispatch + observation
recipe.

## Post-mortems go in the session save

If something goes wrong, the diagnosis + corrective measure goes in a
session save (`mcp__memory__save_state`), not a file in the repo. Repo
files describe how the system currently is and is used, not the
history of how it got here. `docs/PROPOSAL.md` is the one legacy
exception.
