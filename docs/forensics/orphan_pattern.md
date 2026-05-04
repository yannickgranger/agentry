# Forensic snapshot — substrate orphan + verdict-divergence patterns

Cases collected for the lifecycle FSM EPIC (#246) council. Each case shows divergence between what `agentry:verdicts` reports and what the brief actually produced, OR a substrate path that reached terminal without emitting a Failed verdict.

## Case 1 — PR #371 (brf_runner_pivot_migrate_5_complex_roles_to_json_v1)

**Pattern:** false-positive self-review on staged-only diff.

- Coder produced commit `f02e5d5` containing the full 5-role JSON migration: 5 new JSON files, 343 LoC removed from `seed.rs`, all template tokens correctly applied.
- After committing, coder made unrelated post-commit edits (loader.rs, test files, reverted ci-watcher's `${FORGE_WRITE_PERMITS}` to literal in working tree).
- Self-review read STAGED-ONLY diff, missed the prior commit, fired 3 phantom Blocker findings: "unapplied verb: CREATE seed/roles/archaeologist-..." (which IS in the commit).
- Substrate emitted `verdict: failed`, abandoned brief.
- Captain manually adopted: `git reset --hard f02e5d5`, verified acceptance locally (clippy + test + arch-check all green), pushed branch, opened PR #371, merged.

**Divergence:** verdict stream said `failed`; reality was a clean, acceptance-passing commit ready to ship.

## Case 2 — PR #374 (brf_runner_pivot_reviewer_mechanical_port_and_migrate_v1)

**Pattern:** orphan — coder commits cleanly, container dies before terminal event lands.

- Coder produced commit `84a35b9` — full bash → Rust runner port + JSON migration: 7 files, 312 insertions, 87 deletions, including new reviewer_mechanical_runner.rs (197 LoC) + 39 LoC of unit tests + 37-line JSON role file + justfile recipe + seed.rs cleanup.
- Last trace event: 18:12:29.488 (claude assistant message during coder turn).
- No `done` event, no `agent_event:terminated`, no Failed verdict.
- `active_briefs` set was empty after the orphan; brief stuck without recovery path.
- Captain manually adopted: verified acceptance (12+5+4+6+4+1 tests pass), pushed branch, opened PR #374, merged.

**Divergence:** verdict stream emitted nothing; brief silently disappeared from active set.

## Case 3 — PR #381 (brf_phase2_376_auditor_ra_query_stage_v2)

**Pattern:** orphan after rate-limit retry.

- Brief v1 hit Anthropic API rate_limit during coder turn; substrate emitted Failed correctly (clean failure path).
- Captain re-dispatched as v2.
- v2 coder produced commit `eede375` — 3 files, 606 insertions (167 in auditor_claude_runner.rs, 196 in lib.rs, 244 in new tests).
- Last trace event: 20:19:53.046 (claude reply received).
- Same as Case 2: no terminal event, brief disappeared from active set.
- Captain manually adopted: verified acceptance (incl 27 new auditor tests), pushed, PR #381, merged.

**Divergence:** rate_limit retry produced correct work; orphan pattern fired again on the retry.

## Case 4 — PR #382 (brf_phase2_378_coder_callers_gate_v1)

**Pattern:** orphan — same shape as Case 2.

- Coder produced commit `abfd8b7` — 6 files, 820 insertions, 4 deletions: new `precommit_gate.rs` (352 LoC) + tests (272 LoC) + allowlist.toml + spec file + runner integration.
- Last trace event well within wall-clock budget; container died silently.
- Captain manually adopted: 19 new precommit_gate tests + 27 auditor + others pass, pushed, PR #382, merged.

**Divergence:** identical to Case 2 — silent disappearance, correct commit available on disk.

## Case 5 — Brief 232 v2 (historical, from #246 body)

**Pattern:** premature `shipped` verdict masked a stalled chain.

- Substrate's verdict stream wrote `shipped` when reviewer-claude pass returned clean.
- Reviewer-claude actually returned a Warn finding without rerouting, chain stalled before producing PR.
- SETNX gate had locked the first verdict; subsequent terminal events were dropped.
- Operator reading `agentry:verdicts` saw "shipped", but no PR existed on the forge.

## Case 6 — Brief 233 v1 (historical, from #246 body)

**Pattern:** premature `shipped` masked a coder rework failure.

- 12:48 — verdict said `shipped`.
- 13:01 — coder emitted `done failed` with 4 unapplied verbs (rework cycle exhausted).
- Verdict stream still showed `shipped` because SETNX had locked it.

## Common patterns across all 6 cases

| Pattern | Cases | Substrate behavior | Reality |
|---|---|---|---|
| Premature-shipped + masked-failed | 5, 6 | `shipped` locked first, downstream `failed` dropped | Stalled or failed brief reported as success |
| Self-review false-positive | 1 | `failed` emitted on staged-only diff | Clean commit existed, unread by self-review |
| Wall-clock-no-Failed orphan | 2, 3, 4 | No terminal event, brief leaves `active_briefs` silently | Correct commit on disk, no recovery path |

## Implications for lifecycle FSM design

1. **Terminal-only verdicts** — intermediate states (Authoring, Verifying, Reviewing, Shipping, Watching) emit progress events, never verdicts. Eliminates Cases 5, 6.
2. **State as single Redis key** — `agentry:brief:{id}:state` updated atomically. Eliminates the SETNX dedup hack.
3. **Orphan detection** — a brief in any non-terminal state for > wall-clock budget MUST transition to terminal Failed (Aborted) by the daemon's reaper. Eliminates Cases 2, 3, 4.
4. **Self-review reads commit-not-staged** — covered by Case 1; the self-review's diff scope is a separate substrate bug, NOT in the lifecycle FSM scope, but worth filing as a sibling fix.

## Source data

- Trace streams: `agentry:brief:<id>:trace` (Redis stream, retained ~3 days)
- Delivery hashes: `agentry:delivery:<id>` (HSET — pr_number, pr_url, ci_state, merged)
- Verdict markers: `agentry:verdict:emitted:<id>` (1 = SETNX-locked)
- Daemon log: `/tmp/agentry-orchestratord.log` (running daemon)
