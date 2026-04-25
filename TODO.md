# Next

**State of develop**: bootstrap infrastructure for self-hosting in place — per-brief workspace (`BriefWorkspace`), wall-clock timeout from `permit.max_wall_seconds`, `agentry-net` podman network with dedicated `agentry-sccache-redis`. `AgentRole` carries optional `workspace_mount` + `sccache` flag. Permit broker + signed `WorkPermit` + tool-call audit + message-graph routing + chain triggers + cron + webhook intake all land.

**Next one PR** (issue #8 on `yg/agentry`): the dogfood team `agentry-self-host-v0` — `coder-claude-agentry → reviewer-mechanical-agentry → shipper-agentry`, strict sequential, `max_retries=0`. After this PR merges, agentry is the only way to modify agentry. No more Claude-authored direct commits; all further changes flow as briefs dispatched into this team. No break-glass exception.

## Open issues on the forge

| # | Title | Track |
|---|-------|-------|
| [#8](https://agency.lab:3000/yg/agentry/issues/8) | dogfood team — coder + mechanical reviewer + shipper + v0 topology | **bootstrap — last Claude-authored PR, cutoff trigger** |
| [#9](https://agency.lab:3000/yg/agentry/issues/9) | ci-watcher + auto-merge on CI-green | self-host — dispatched as brief |
| [#10](https://agency.lab:3000/yg/agentry/issues/10) | bare-clone + git-worktree (replaces minimal workspace) | self-host |
| [#11](https://agency.lab:3000/yg/agentry/issues/11) | `ReworkNeeded` verdict + `ReviewFinding` + coder↔reviewer loop | self-host |
| [#12](https://agency.lab:3000/yg/agentry/issues/12) | `reviewer-claude-agentry` — LLM reviewer alongside mechanical | self-host |
| [#13](https://agency.lab:3000/yg/agentry/issues/13) | DAG scheduler + concurrent brief execution | self-host |
| [#14](https://agency.lab:3000/yg/agentry/issues/14) | first cfdb ban rule — no `.unwrap()` in non-test prod | self-host |

Bodies on each issue carry verb-targeted transformation lists (`CREATE`/`UPDATE`/`DELETE`/`REPLACE`/`MOVE` + `crate`/`file`/`line`) and real-infra acceptance criteria. The last six become brief payloads after #8 lands.

## Replay (current shape)

```bash
cd /var/mnt/workspaces/agentry

export AGENTRY_REDIS_PASSWORD=$(cat ~/.config/agentry/redis.password)
export AGENTRY_REDIS__URL="redis://:${AGENTRY_REDIS_PASSWORD}@127.0.0.1:6380"
export AGENTRY_DASHBOARD__PORT=7800
export AGENTRY_WEBHOOK__SECRET=$(openssl rand -hex 16)
export GITEA_TOKEN=$(keepassxc-cli show -sa Password --no-password -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx "gitea/agency.lab")

# Infra.
just dev-redis-up
just agentry-net-up

# Build + seed.
cargo build --release --workspace
./target/release/orchestrator key-gen --force || true
./target/release/orchestrator seed

# Daemons.
pkill -9 -f '/target/release/orchestratord' || true
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
disown
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
disown

# Regression smoke (the three probe teams).
./target/release/orchestrator submit examples/verify-M0.json            # echo-team → shipped
./target/release/orchestrator submit examples/verify-M3.json            # naughty-team → permit_violation
# Workspace + timeout probes run from seed-based teams:
#   workspace-probe-team  — verifies bind-mount lifecycle
#   timeout-probe-team    — verifies wall-clock enforcement
#   sccache-probe-team    — verifies agentry-net + sccache-redis round-trip

redis-cli -p 6380 -a "$AGENTRY_REDIS_PASSWORD" --no-auth-warning XREVRANGE agentry:verdicts + - COUNT 5
```

## Arch gate

Every PR runs both:

```bash
scripts/arch-check.sh
# → graph-specs check --specs specs/concepts/ --code crates/     (0 violations required)
# → cfdb extract --workspace . --db .cfdb/db-local --keyspace agentry
# → cfdb violations (one .cypher per file in .cfdb/queries/; currently empty by design)
```

Adding, renaming, or removing a top-level `pub struct/enum/trait/type` without updating `specs/concepts/` in the same PR is a CI failure. Ban rules land one-per-PR with their own justification and zero existing violations.

## If this session ends mid-task

- `git status` → `fix: ...` or `feat: ...` commit (no `wip:`).
- `mcp__memory__save_state namespace='a0-session:<date>-<topic>'` with a structured handoff.
- Next session: `mcp__memory__load_state` + check PR merge states on the forge + read this file.

## History (summarised)

Prior to the current state, the project landed in a 2-hour burst of M0-M8 theater milestones on a dead feature branch, all demonstrating shell scripts emitting what their own verify greps expected. That work was reframed: specs + cfdb gate installed (PR #4), per-agent Containerfiles consolidated into inline scripts + `package_manager` + `entrypoint_script` (PRs #2 then #15), infra for real self-hosting added (PRs #18 = recovered #16 + #17).

The milestone-number narrative does not return. Every future change is a verb-targeted brief against a specced codebase.
_poc_v5: 2026-04-25_
