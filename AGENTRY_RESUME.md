# agentry — session resume

Canonical per-session state for any future Claude session picking this up.

## What this project is

A minimal orchestrator for ephemeral agent containers. Every containerised agent speaks the same NDJSON stdin/stdout protocol; the daemon reads `Brief`s off `agentry:briefs`, resolves a `TeamTopology` + `AgentRole`s, mints signed `WorkPermit`s, spawns one container per role on a stock public image, enforces the permit on every `tool_call`, routes inter-role `Message` events, records a verdict.

Full architectural read: `README.md` → `specs/concepts/*.md` (the DDD concept list enforced by `graph-specs-rust`).

## The cutoff rule

`agentry` is built by `agentry`. Once issue #8 on `yg/agentry` merges, Claude cannot author code on `yg/agentry` directly. Every further change is a brief dispatched into the `agentry-self-host-v0` team defined in that PR (`coder-claude-agentry → reviewer-mechanical-agentry → shipper-agentry`).

**No break-glass exception.** If agentry breaks, root-cause it from the trace stream + orchestratord log + stderr, then either (a) convince the user case-by-case that a one-off direct fix is warranted, or (b) file another brief with more precise instructions.

Verify where we are:

```bash
# PR #8 merge status on the forge.
curl -sk https://agency.lab:3000/api/v1/repos/yg/agentry/pulls/8 \
    -H "Authorization: token $(keepassxc-cli show -sa Password --no-password \
        -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx 'gitea/agency.lab')" \
    | jq '{state, merged, merged_at}'
```

If `merged: true` — cutoff is live, no direct commits. If still open — the pre-cutoff rules (feature branch + PR + CI green) still apply.

## Redis + infra

| Resource | Endpoint | Owner |
|---|---|---|
| Dev Redis (briefs, verdicts, trace, audit, registry) | `127.0.0.1:6380` inside `agentry-dev-redis` podman container | `just dev-redis-up` |
| sccache backend | `agentry-sccache-redis:6379` on `agentry-net` network (no host port) | `just agentry-net-up` |
| Ed25519 signing key | `~/.config/agentry/signing.key` (0600) | `orchestrator key-gen` |
| Redis password | `~/.config/agentry/redis.password` (0600) | `just dev-redis-up` generates it once |
| Gitea | `https://agency.lab:3000/` (token at keepassxc `gitea/agency.lab`) | external |
| Workspace root | `/var/mnt/workspaces/agentry-work/briefs/<brief_id>/` | daemon allocates lazily |

**Never** point agentry's dev runs at a production Redis. Pin tests in `crates/orchestrator-runtime/src/config.rs` already reject `192.168.1.152` and `192.168.1.189` as hardcoded targets; the default is `127.0.0.1:6380`.

## Resumption protocol

Fresh Claude session picking this up MUST, in order:

1. `mcp__memory__load_state namespace='a0-session:<latest-agentry-date>'` — the canonical per-session handoff record. If absent, read `mcp__memory__get key='project:agentry:resume'` for the pointer.
2. `git fetch origin develop` in `/var/mnt/workspaces/agentry`; compare local `origin/develop` to the forge's `develop` branch. If they diverge (e.g. a stacked PR merged into a feature branch instead of develop), that's an orphan-gotcha — recover via cherry-pick onto develop in a fresh PR before any new feature work.
3. Check open issues + PRs on `yg/agentry`. If PR #8 is open: pre-cutoff, continue. If merged: cutoff is live, any new work goes through `agentry-self-host-v0` via `orchestrator submit`.
4. `scripts/arch-check.sh` — verify the spec + cfdb gates still pass locally before touching anything.
5. Pick up from the single "next one PR" in `TODO.md` or from a brief plan in the saved session state.

Do not invent new primitives, add new crates, or re-open deferred features. If tempted, stop and re-read the frozen rules below.

## Frozen rules

**Terminal:**
- Claude is Claude Max subscription only — never the per-token Anthropic API. Every Claude-driven role subprocesses the host's `claude` CLI. Grok and Gemini APIs are fine.
- Dev Redis is local podman only — never prod LXCs.
- Every change flows through a PR against `develop` with CI green. Never bypass hooks or push to main/develop directly.
- After PR #8 merges, no Claude-authored code on `yg/agentry`. No break-glass.

**Process:**
- When user questions a choice: answer WHY first, wait for them to redirect, don't reverse-pitch.
- When unsure: "I don't know, stopping." No pitched alternatives.
- Factual commits. No milestone numbering. No celebratory language.
- Stacked PRs: rebase the stacked PR onto develop the instant the base PR is approved for merge. Never merge a stacked PR after its base has already merged — it orphans the feature branch and the commits never reach develop.

**Methodology:**
- Every change that adds, renames, or removes a top-level `pub struct/enum/trait/type` must update `specs/concepts/*.md` in the same PR. Arch gate enforces.
- Ban rules (`.cfdb/queries/*.cypher`) arrive one per PR, each with its own justification, zero existing violations at introduction. No baseline file, no ratchet, no allowlist.
- Bug fixes that reach pub surface follow the same rule.

**Brief discipline (pre- and post-cutoff):**
- Before dispatching any brief: read the forge issue, read the affected source, query cfdb for affected symbols, read the relevant `specs/contexts/` or `specs/concepts/` file. If the spec is missing or stale for what the brief would touch, spec update goes first (either a precursor brief or folded into the scope).
- Brief payloads use verbs `CREATE / DELETE / REPLACE / UPDATE / MOVE` + `crate:file:line` targets. Free-form "fix this issue" briefs are forbidden.
- See `docs/dogfood-protocol.md` for the example payload shape and the dispatch recipe.

## Known limitations (current develop)

- **Single-daemon.** `orchestratord` uses `XREAD BLOCK $` — no consumer groups. Running two daemon processes against the same Redis double-processes every brief. Issue #13 introduces concurrent brief execution; multi-daemon consumer groups are a later step.
- **Sequential role iteration.** Within a single brief, the daemon loops over `team.roles` in declaration order — the DAG walk of `message_graph` lands in #13.
- **No rework loop.** Reviewer's verdict is boolean (`Shipped` or `Failed`); structured findings + coder↔reviewer iteration come in #11.
- **Mechanical reviewer only.** LLM review (`reviewer-claude-agentry`) arrives in #12 after `ReviewFinding` lands.
- **No ci-watcher.** Shipper opens a PR; a human merges it after CI goes green. Auto-merge lands in #9.
- **Minimal workspace.** Coder `git clone`s inside its container per brief. Bare-clone + `git worktree add` is #10.
- **No MCP server mounting at runtime.** `AgentRole.mcp_servers` is declared but the spawner ignores it today.
- **`fetch_role_any_version` iterates v1..v5 and picks the first hit.** Later versions lose to earlier. Will bite when a role's version gets bumped. Not yet filed as an issue; fix is trivial.
- **Shipper's `permit_scope` is symbolic.** `forge:write:yg/agentry` in the scope is not runtime-checked against `brief.payload.repo`. A brief with a different repo in the payload would still push there if the shipper's GITEA_TOKEN authorises it. Internal trust only until it becomes a real concern.

## Previous state (for diffing)

`docs/PROPOSAL.md` contains the 2026-04-23 archaeology proposal that seeded this project. It describes a roadmap in M0-M9 milestones. That framing is retired: the project now plans in GitHub-issue-sized transformations with explicit verbs + file:line targets, not in milestones. The proposal is kept for the motivation + context it encodes, not for its roadmap.

## Last updated

See `git log -1 AGENTRY_RESUME.md`. If this document drifts from the code, the code is right and this document is a bug to fix via a brief.
