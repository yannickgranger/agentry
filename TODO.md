# Next Concrete Action

**Status:** **M0 → M8 ALL GREEN.** Eleven verifies pass end-to-end on real infra + local podman Redis.

Next: **M9 — first real qbot-core issue closed end-to-end** (the "we are up and running" gate from the original roadmap).

## HARD RULES (re-read every session)

> **Redis: LOCAL podman only** (`127.0.0.1:6380`). Never LXC 401 (.152) or 522 (.189).
> **Claude: Max subscription only** (host `claude` CLI + bind-mounted creds). Never Anthropic API.
> **Cheap APIs OK:** Grok (xAI), Gemini (Google). Per-role `passthru_env`.
> **PR-opening token** (`GITEA_TOKEN`) is per-role via `passthru_env`. Never broad-cast.

## Done — all 8 roadmap milestones + figment + prod-Redis fix

| # | Scope | Commit | Proof |
|---|-------|--------|-------|
| M0 | runtime + podman + echo | `24f7e4f` | verdict shipped |
| M1 | dashboard + SSE | `eb51c08` | /sse/verdicts live |
| M2 | registry editor (forms) | `2753ecf` | POST /roles /teams /projects |
| M3 | permit broker | `95dc1cb` | permit_violation on unauthorized tool |
| M4 | inter-role message routing | `366b1b9` | speaker → listener |
| M5a | cheap-API LLM (xAI Grok) | `38852b8` | grok-4-fast returned pong |
| M5b | Claude Max via host CLI | `06a922e` | claude -p returned pong, zero API spend |
| M6 + prod-Redis fix | permit_overrides narrow scope + local podman Redis | `89bc95f` | fs:write scope denied; 29 keys wiped from .152 |
| figment | typed config (defaults + TOML + env overlay) | `bf3a81b` | 30 tests incl. anti-prod-Redis pin |
| M7 | shipper opens real PR on toy repo | `a72fe71` | PR #1 on yg/agentry-toy |
| M8 | triggers (cron + webhook + chain) | see `git log` | chain A→B both shipped; webhook 401-unauth and 200-submit |

## Replay (in order)

```bash
cd /var/mnt/workspaces/agentry

export AGENTRY_REDIS_PASSWORD=$(cat ~/.config/agentry/redis.password)
export AGENTRY_REDIS__URL="redis://:${AGENTRY_REDIS_PASSWORD}@127.0.0.1:6380"
export AGENTRY_DASHBOARD__PORT=7800
export AGENTRY_WEBHOOK__SECRET=$(openssl rand -hex 16)
export GITEA_TOKEN=$(keepassxc-cli show -sa Password --no-password -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx "gitea/agency.lab")
export XAI_API_KEY=$(keepassxc-cli show -sa Password --no-password -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx "services/xai-grok")

cargo build --release --workspace
./target/release/orchestrator key-gen --force || true
just dev-redis-up
for img in echo naughty speaker listener grok claude synthesizer narrowed-coder shipper; do
  podman image exists localhost/agentry/${img}-agent:v1 \
    || (cd containers/${img}-agent && podman build -t agentry/${img}-agent:v1 -f Containerfile .)
done
./target/release/orchestrator seed

ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1

for m in 0 1 2 3 4 5a 5b 6 7; do just verify-M$m; done
just verify-M8-chain
just verify-M8-webhook
```

## M9 — first real qbot-core issue end-to-end

This is the north-star "we are up and running" gate.

### Prerequisites (resolve with user before building)

1. **Which qbot-core issue?** Target should be:
   - Small, well-specced (typo fix, one clippy warning, one test rename).
   - Touches ≤3 files.
   - Has clear acceptance (`cargo clippy` clean, `cargo test` green).
2. **Which coder model?**
   - **Claude Max** via M5b substrate (`claude-echo` role template) — free under subscription.
   - **Grok Code Fast** via M5a substrate — cheap but untested on real Rust.
   - Lean: start with Claude Max; it's what the user already trusts.
3. **Team topology for qbot-core work.** Proposal:
   ```
   archaeologist -> prescriber -> coder-claude -> reviewer -> shipper
   ```
   - `archaeologist`: read-only, uses grep/find on a cloned worktree. Produces facts.
   - `prescriber`: read-only, synthesizes REUSE/CREATE decisions + `permit_overrides.fs_write` to scope coder.
   - `coder-claude`: Claude Max in container, writes files within narrowed scope, runs `cargo check`.
   - `reviewer`: reads diff, runs full `cargo test`, emits `approve` or `reject`.
   - `shipper`: M7 shipper retargeted at `yg/qbot-core`, opens PR.
4. **Do we merge automatically?** Lean: NO for M9. Shipper opens PR; human merges. Auto-merge is a separate scope.

### Architectural work needed (not just config)

- **Cloned worktree per team.** Each agent needs the qbot-core checkout. Options:
  a) One worktree shared via bind mount (unsafe — parallel writes, which isn't a concern since roles are sequential, but still cross-context contamination).
  b) Each agent clones its own. Wasteful but clean.
  c) First role clones; subsequent roles bind-mount the same directory.
  Lean: (c) — clone once in `archaeologist`, bind-mount through team.
  Needs: a way to mark a role as "persists its workspace" + a way for downstream roles to inherit it. This is a NEW CONCEPT. Deferring to explicit design discussion before coding.
- **Real quality signal.** `cargo check`/`cargo test` from a container takes minutes for qbot-core. Need resource allocation + timeout (podman `--timeout`), separate from the brief-level budget.
- **MCP server for forge ops inside coder-claude.** Claude needs to read file history / blame / diff. Lean: mount `mcp-forge` into coder-claude's container (M5b-like pattern + mounts field already exists).

### Alternatives to M9

If M9 feels too ambitious as the next step, three intermediate milestones that are each individually valuable:

- **M8.5 — parallel spawning** in one team. Currently all roles run sequentially. For real teams this is fine for linear pipelines but limits council patterns (archaeologist + spec-guardian + cfdb-auditor running in parallel). ~150 LOC.
- **M8.6 — persistent workspace inheritance.** A role declares `workspace: persist`; downstream roles declare `workspace: inherit`, and the spawner bind-mounts the upstream container's workspace as read-only into the downstream. ~100 LOC.
- **M8.7 — glob matching for fs scope.** Currently M6 does literal-path matching. For real work we need `fs:write:/workspace/src/**` style globs. Add `globset` crate. ~50 LOC.

Recommend building at least M8.6 before M9 — qbot-core work really needs persistent workspace across roles.

## If this session ends mid-task

- `git status` → commit as `wip(m9): ...`.
- Update this TODO.
- `mcp__memory__set key="project:agentry:resume"`.
- Replay block above before new work.
