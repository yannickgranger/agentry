# Next Concrete Action

**Status:** **M0 + M1 + M2 + M3 + M4 + M5a GREEN** as of 2026-04-23. Next: M5b — Claude Max via host `claude` CLI (only if M5b is worth it; M5a already proves the LLM path works cheap).

## HARD RULE

> **Claude is Claude Max subscription only. NEVER the per-token Anthropic API.**
> Any Claude agent subprocesses the host's `claude` CLI. No `ANTHROPIC_API_KEY`, no `anthropic` Python SDK, no HTTP to `api.anthropic.com`, EVER.
> **Per-token APIs OK for cheap/fast models only: Grok (xAI) ✓ working, Gemini (Google).**

## Done

| # | Scope | Commit | Proof |
|---|-------|--------|-------|
| M0 | runtime + podman spawner + echo | `24f7e4f` | verdict shipped |
| M1 | dashboard + SSE | `eb51c08` | /sse/verdicts live |
| M2 | registry editor (forms) | `2753ecf` | POST /roles /teams /projects |
| M3 | permit broker (signing + enforcement) | `95dc1cb` | permit_violation on unauthorized tool |
| M4 | inter-role message routing | `366b1b9` | speaker → listener via team_context |
| M5a | cheap-API LLM agent (xAI Grok) | see `git log` | grok-echo container returned "pong"; verdict shipped; tokens_in=163, tokens_out=1 |

### Replay (M0 → M5a)

```bash
export AGENTRY_REDIS_URL='redis://:RedisRationalized2026@192.168.1.152:6379'
export AGENTRY_DASHBOARD_PORT=7800
# XAI_API_KEY must be set in orchestratord's env. From kdbx:
export XAI_API_KEY=$(keepassxc-cli show -sa Password --no-password \
  -k ~/agent-zero/key/claude.key ~/agent-zero/claude.kdbx "services/xai-grok")

cd /var/mnt/workspaces/agentry
cargo build --release --workspace
./target/release/orchestrator key-gen --force

for img in echo naughty speaker listener grok; do
  podman image exists localhost/agentry/${img}-agent:v1 \
    || (cd containers/${img}-agent && podman build -t agentry/${img}-agent:v1 -f Containerfile .)
done
./target/release/orchestrator seed

ps -eo pid,comm | awk '$2 ~ /^orchestrator/ {print $1}' | xargs -r kill -9
nohup ./target/release/orchestratord         > /tmp/agentry-orchestratord.log 2>&1 &
nohup ./target/release/orchestrator-dashboard > /tmp/agentry-dashboard.log 2>&1 &
sleep 1

for m in 0 1 2 3 4; do just verify-M$m; done
just verify-M5a
```

## M5b — Claude Max via host `claude` CLI (optional; M5a already validates the LLM path)

Goal: an agent whose container subprocesses the host's `claude -p` and emits the reply — no Anthropic API spend.

### Open questions to resolve with user BEFORE building

1. **Where does Claude Max store auth on this machine?** `~/.claude/`? `~/.config/claude/`? Keychain/Secret Service?
2. **Can the auth be bind-mounted into a rootless podman container read-only?** `podman run -v ~/.claude:/root/.claude:ro` works if auth is file-based.
3. **Is the `claude` binary installed on the host?** Check `which claude`. If not, install before M5b.
4. **Claude Max rate limit behaviour.** Running many concurrent claude agents might trip limits. For M5b MVP: single agent at a time, sequential.

### If answers are green

- Container: `alpine + jq` + either bind-mount the host `claude` binary or install it at build time.
- Entrypoint mirrors `grok-agent/entrypoint.sh` but shells out to `claude -p "<prompt>"` instead of `curl`.
- `claude-echo` role with `passthru_env: ["CLAUDE_CONFIG_DIR"]` (whatever env var `claude` reads to find auth), bind-mount via a new `AgentRole.mounts` field (add it to types + dashboard form — M5b type change).

### If answers are red (e.g. auth can't leave host)

Plan B: a tiny localhost-bound HTTP shim running on the host. Agent container `curl`s `http://host.containers.internal:NNNN/complete` with the prompt; shim invokes `claude -p` and returns. Zero auth crossing. Shim is maybe 50 LOC.

## M6+ backlog (per roadmap — deferred)

| # | Scope |
|---|-------|
| M6 | Upstream `permit_overrides` narrow downstream scope (synthesizer → coder pattern) |
| M7 | E2E on toy repo with shipper role (first PR opened by agentry) |
| M8 | Project + triggers (cron / webhook) |
| M9 | First real qbot-core issue closed by agentry |

## If this session ends mid-task

- `git status` → commit as `wip(m5b): ...`.
- Update this `TODO.md`.
- `mcp__memory__set key="project:agentry:resume" value=<updated>`.
- Replay block at top before new work.
