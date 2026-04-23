# User Feedback — Hard Rules Lived Through Pain

**Source keys:**
- `memory:feedback:no-split-brains-no-drift`
- `memory:feedback:dogfood-cfdb-on-cfdb`
- `memory:feedback:cfdb-public-oss-bar`
- `memory:feedback:architect-review-reads-existing-docs`
- `memory:feedback:no-manual-sha-ceremony`
- `memory:feedback:workflow-rfc-spec-issues-impl`
- `memory:feedback:rust-fmt-before-push`
- `memory:policy:redis-only`
- `feedback:cfdb:architecture-gate-not-a-bug`
- `feedback:cfdb:chronological-priority`
- `feedback:cfdb:no-backlog-inflation`
- `feedback:cfdb:cross-dogfood-is-fact-locking`
- `feedback:cfdb:develop-to-develop-cross-dogfood`
- `feedback:cfdb:no-sha-dance`
- `feedback:cfdb:memory-in-redis-not-files`
- `feedback:graph-specs:no-pr-before-architect-review`
- `feedback:graph-specs:one-question-no-admin`
- `feedback:graph-specs:wip-pr-pattern`

## Why interesting for v2
Every entry here is a documented user frustration. The corrective rules apply across ANY project this user runs. v2 must hard-code them.

## The core directive: "no split-brains / no drift"
User quoted: **"i dont want no more split-brains / drift"**
This IS the cfdb principle, but also the meta-principle for v2.

Violations enumerated:
> two modules defining the same concept independently, two parser/scanner state machines tracking the same state, two resolution points for a name/alias, two impls of the same port across adapters that silently no-op vs do real work, new extractor crate emitting 'overlapping but different' facts without cross-check gate, new CLI/MCP tool parsing a domain enum with hardcoded list instead of FromStr, hand-maintained ceiling/baseline files ratcheting against moving target.

## Dogfood from day zero
> cfdb is a code-facts database for Rust workspaces — run it against its own source tree, wire it into CI, and let the findings drive priority.
> If we do not trust our own tool on our own code, nobody else will.
> User directive: "you need to dogfood each time possible"

For v2 orchestrator: orchestrate the building of the orchestrator. The dashboard proves the engine. Self-referential from day zero.

## The binary does the ceremony, not the human
From `feedback:cfdb:cross-dogfood-is-fact-locking`:
> when user says "X is the binary's job," the instinct is to push responsibility from human ceremony (Cargo.toml edits, PR lockstep choreography) into the tool itself.

Lockstep SHA updates, weekly bump crons, pinned cross-fixture files — all dismissed by the user as "humans doing what the binary should do."

v2 rule: if a workflow requires a human to manually sync two SHAs, the tool is wrong.

## Minimal-ceremony PRs
From `feedback:cfdb:develop-to-develop-cross-dogfood`:
> cross-dogfood can be repo A -> develop, repo B -> develop, they take the latest and we dont need commit SHA
> Trusting develop and failing loudly when companion's develop drifts is the simpler invariant that scales better.

## "we wont grow backlog to infinity"
From `feedback:cfdb:no-backlog-inflation`:
> Don't default to filing new tracker issues for drift, tech debt, or quality-metric violations surfaced by gates.
> Prefer fix-in-place (boy-scout ≤15 min), inline "Known pre-existing" note in PR body, or silent leave-alone over creating new issues.

Cumulative issue-filing expands backlog faster than it drains. Anti-goal.

## No manual SHA dance — visceral reaction
From `memory:feedback:no-manual-sha-ceremony`:
> User reaction: "hey what are we doing changing SHA in PR for 10x what the fuck is that???"
> "I DONT GIVE A SHIT. DONT WANT TO MANUALLY CHANGE SHA EVERY 30 MIN THEN WAIT FOR CI AND REPEAT YOUR STUPID DESIGN"

Rule: content-identical SHAs → pick ONE (branch HEAD, never post-merge commit).

## RFC → Spec → Issues → Implementation (ordering is load-bearing)
From `memory:feedback:workflow-rfc-spec-issues-impl`:
> architects write rfc -> spec -> issues -> implem
> ALL issues are in a RFC

Orphan issues (filed without RFC) are drift. Retroactively absorbed before implementation continues.

**Team rule:** architect work happens in TeamCreate team, NOT one-shot Agent calls.
Minimum team: solid-architect + clean-arch. Add rust-systems + ddd-specialist as scope needs.

## Architect review required prior reading
Every architect review prompt MUST enumerate:
1. All prior RFCs (file paths)
2. Council verdicts (if repo has them)
3. CLAUDE.md (repo-local methodology)
4. Spec files
5. Current open-issue backlog (inline or via forge query)
6. Sibling-repo context

Verdict without citations → auto-REJECTED.

## Redis-only memory
From `memory:policy:redis-only`:
> User directive 2026-04-20: "this is a drift: NO file based memory. Only Redis"

Key naming convention for v2:
- `memory:feedback:<slug>`
- `memory:project:<slug>`
- `memory:session:<date>-<slug>`
- `memory:policy:<slug>`
- `state:a0-session:<date>-<uuid>`

## WIP PRs + architect review before merge
From `feedback:graph-specs:no-pr-before-architect-review` + `wip-pr-pattern`:
> When user says "PR the RFC", run the four-lens architect-team review FIRST, then PR.
> Open PRs as WIP (title prefix "WIP:" on Gitea, or draft on GitHub) when work is unfinished.
> Default: "Running architects (~10 min) then PR. Skip?" — give explicit choice.

## One-question-at-a-time
From `feedback:graph-specs:one-question-no-admin`:
> When multiple decisions block work, ask user one question at a time in plain prose.
> No decision tables, no OQ enumerations, no umbrella-issue mockups.
> User called out "administrative giggle" after 4-question OQ list.

## Public-repo quality bar
From `memory:feedback:cfdb-public-oss-bar`:
> all planet will be watching you
> Every artefact a stranger opening the repo sees is part of the product: README standalone-intelligible, CI workflow names, commit messages, PR bodies, issue titles all public.

## Architecture gate failures are real bugs, not noise
From `feedback:cfdb:architecture-gate-not-a-bug`:
> When quality-all reports the architecture gate FAIL with empty output and short duration, DO NOT dismiss it as a known tool bug.
> Always run audit-split-brain directly to read the human-readable finding.

## cargo fmt before every push
From `memory:feedback:rust-fmt-before-push`:
> why do you want to continue something when you dont even format correctly
Run `cargo fmt --all -- --check` locally BEFORE git push. Always.
